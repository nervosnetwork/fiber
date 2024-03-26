use log::{debug, error, info, warn};
use std::{
    collections::{HashMap, HashSet},
    str,
    sync::Arc,
};
use tentacle::{
    async_trait,
    builder::{MetaBuilder, ServiceBuilder},
    bytes::Bytes,
    context::{ProtocolContext, ProtocolContextMutRef, ServiceContext},
    secio::PeerId,
    service::{
        ProtocolHandle, ProtocolMeta, ServiceAsyncControl, ServiceError, ServiceEvent,
        TargetProtocol,
    },
    traits::{ServiceHandle, ServiceProtocol},
    ProtocolId, SessionId,
};
use tokio::select;
use tokio::sync::{mpsc, Mutex};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use super::{types::PCNMessage, CkbConfig, Command, Event};

const PCN_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

#[derive(Clone, Debug)]
struct PHandle {
    state: SharedState,
}

impl PHandle {
    fn new(state: SharedState) -> Self {
        Self { state }
    }

    fn create_meta(self, id: ProtocolId) -> ProtocolMeta {
        MetaBuilder::new()
            .id(id)
            .service_handle(move || {
                let handle = Box::new(self);
                ProtocolHandle::Callback(handle)
            })
            .build()
    }

    async fn send_event(&self, event: Event) {
        let _ = self.state.event_sender.send(event).await;
    }
}

#[async_trait]
impl ServiceProtocol for PHandle {
    async fn init(&mut self, _context: &mut ProtocolContext) {}

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, version: &str) {
        let session = context.session;
        info!(
            "proto id [{}] open on session [{}], address: [{}], type: [{:?}], version: {}",
            context.proto_id, session.id, session.address, session.ty, version
        );
        self.send_event(Event::PeerConnected(context.session.address.clone()))
            .await;

        let peer_id = context.session.remote_pubkey.clone().map(Into::into);
        match peer_id {
            Some(peer_id) => {
                let mut peer_state = self.state.peers.lock().await;
                debug!("Trying to save session of peer {:?}", peer_id);
                let peer = peer_state.entry(peer_id).or_default();
                peer.sessions.insert(context.session.id);
            }
            _ => {
                warn!("Connected to a peer without public key");
                return;
            }
        }
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        info!(
            "proto id [{}] close on session [{}]",
            context.proto_id, context.session.id
        );
        self.send_event(Event::PeerDisConnected(context.session.address.clone()))
            .await;

        let peer_id = context.session.remote_pubkey.clone().map(Into::into);
        match peer_id.as_ref() {
            Some(peer_id) => {
                let mut peer_state = self.state.peers.lock().await;
                debug!("Trying to save session of peer {:?}", peer_id);
                let peer = peer_state.get_mut(peer_id);
                match peer {
                    Some(peer) => {
                        peer.sessions.remove(&context.session.id);
                        if peer.sessions.is_empty() {
                            debug!(
                                "Peer {:?} disconnected, the last session was {}",
                                peer_id, context.session.id
                            );
                            peer_state.remove(peer_id);
                        }
                    }
                    None => {
                        warn!(
                            "Trying to reomve a peer not recorded in peer state {:?}",
                            peer_id
                        )
                    }
                }
            }
            _ => {
                warn!("Disconnected from a peer without public key");
                return;
            }
        }
    }

    async fn received(&mut self, context: ProtocolContextMutRef<'_>, data: Bytes) {
        info!(
            "received from [{}]: proto [{}] data {:?}",
            context.session.id,
            context.proto_id,
            hex::encode(data.as_ref()),
        );

        macro_rules! unwrap_or_return {
            ($expr:expr, $msg:expr) => {
                match $expr {
                    Ok(val) => val,
                    Err(err) => {
                        error!("{}: {:?}", $msg, err);
                        return;
                    }
                }
            };
        }

        let msg = unwrap_or_return!(PCNMessage::from_molecule_slice(&data), "parse message");
        match msg {
            PCNMessage::TestMessage(test) => {
                debug!("Test message {:?}", test);
            }
            PCNMessage::OpenChannel(open_channel) => {
                debug!("Openning channel {:?}", open_channel);
            }
            _ => {
                error!("Message handling for {:?} unimplemented", msg);
            }
        }
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}

#[derive(Clone, Debug)]
struct SHandle {
    state: SharedState,
}

impl SHandle {
    fn new(state: SharedState) -> Self {
        Self { state }
    }

    async fn send_event(&self, event: Event) {
        let _ = self.state.event_sender.send(event).await;
    }
}

#[async_trait]
impl ServiceHandle for SHandle {
    async fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        self.send_event(Event::ServiceError(error)).await;
    }
    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        self.send_event(Event::ServiceEvent(event)).await;
    }
}

#[derive(Clone, Debug)]
pub struct SharedState {
    peers: Arc<Mutex<HashMap<PeerId, PeerInfo>>>,
    event_sender: mpsc::Sender<Event>,
}

impl SharedState {
    fn new(event_sender: mpsc::Sender<Event>) -> Self {
        Self {
            peers: Arc::new(Mutex::new(HashMap::new())),
            event_sender,
        }
    }
}

struct NetworkState {
    control: ServiceAsyncControl,
    shared_state: SharedState,
    token: CancellationToken,
    command_receiver: mpsc::Receiver<Command>,
}

impl NetworkState {
    fn new(
        control: ServiceAsyncControl,
        shared_state: SharedState,
        token: CancellationToken,
        command_receiver: mpsc::Receiver<Command>,
    ) -> Self {
        Self {
            control,
            shared_state,
            token,
            command_receiver,
        }
    }

    async fn process_command(&self, command: Command) {
        debug!("Processing command {:?}", command);
        match command {
            Command::ConnectPeer(addr) => {
                // TODO: It is more than just dialing a peer. We need to exchange capabilities of the peer,
                // e.g. whether the peer support some specific feature.
                // TODO: If we are already connected to the peer, skip connecting.
                debug!("Dialing {}", &addr);
                let result = self.control.dial(addr.clone(), TargetProtocol::All).await;
                if let Err(err) = result {
                    error!("Dialing {} failed: {}", &addr, err);
                }
            }

            Command::SendPcnMessage(peer_id, message) => {
                let peer_state = self.shared_state.peers.lock().await;
                let peer = peer_state.get(&peer_id);
                match peer {
                    Some(peer) => {
                        for session_id in &peer.sessions {
                            let result = self
                                .control
                                .send_message_to(
                                    *session_id,
                                    PCN_PROTOCOL_ID,
                                    message.to_molecule_bytes(),
                                )
                                .await;
                            if let Err(err) = result {
                                error!("Sending message to session {} failed: {}", session_id, err);
                            }
                        }
                    }
                    None => {
                        warn!("Trying to send message to unknown peer {:?}", peer_id);
                    }
                }
            }
        }
    }

    async fn run(mut self) {
        loop {
            select! {
                _ = self.token.cancelled() => {
                    debug!("Cancellation received, shutting down tentacle service");
                    let _ = self.control.shutdown().await;
                    break;
                }
                command = self.command_receiver.recv() => {
                    match command {
                        None => {
                            debug!("Command receiver completed, shutting down tentacle service");
                            let _ = self.control.shutdown().await;
                            break;
                        }
                        Some(command) => {
                            self.process_command(command).await;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct PeerInfo {
    sessions: HashSet<SessionId>,
}

pub async fn start_ckb(
    config: CkbConfig,
    command_receiver: mpsc::Receiver<Command>,
    event_sender: mpsc::Sender<Event>,
    token: CancellationToken,
    tracker: TaskTracker,
) {
    let kp = config
        .read_or_generate_secret_key()
        .expect("read or generate secret key");
    let pk = kp.public_key();
    let shared_state = SharedState::new(event_sender);
    let mut service = ServiceBuilder::default()
        .insert_protocol(PHandle::new(shared_state.clone()).create_meta(PCN_PROTOCOL_ID))
        .key_pair(kp)
        .build(SHandle::new(shared_state.clone()));
    let listen_addr = service
        .listen(
            format!("/ip4/127.0.0.1/tcp/{}", config.listening_port)
                .parse()
                .expect("valid tentacle address"),
        )
        .await
        .expect("listen tentacle");

    info!(
        "Started listening tentacle on {}/p2p/{}",
        listen_addr,
        PeerId::from(pk).to_base58()
    );

    let control = service.control().to_owned();

    tracker.spawn(async move {
        service.run().await;
        debug!("Tentacle service shutdown");
    });

    tracker.spawn(async move {
        NetworkState::new(control, shared_state, token, command_receiver)
            .run()
            .await;
    });
}
