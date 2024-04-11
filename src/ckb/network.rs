use log::{debug, error, info, warn};
use ractor::{async_trait as rasync_trait, forward, Actor, ActorProcessingErr, ActorRef};
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
    traits::{self, ServiceHandle, ServiceProtocol},
    ProtocolId, SessionId,
};
use tokio::select;
use tokio::sync::{mpsc, Mutex};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    ckb::{
        channel::{
            ChannelCommand, COUNTERPARTY_INITIAL_COMMITMENT_NUMBER, DEFAULT_COMMITMENT_FEE_RATE,
            DEFAULT_FEE_RATE, DEFAULT_MAX_ACCEPT_TLCS, DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
            DEFAULT_MIN_TLC_VALUE, DEFAULT_TO_SELF_DELAY, HOLDER_INITIAL_COMMITMENT_NUMBER,
        },
        types::AcceptChannel,
    },
    Config,
};

use crate::unwrap_or_return;

use super::{
    channel::{ChannelActorState, ChannelEvent, ProcessingChannelError, ProcessingChannelResult},
    command::{self, PCNMessageWithPeerId},
    types::{Hash256, OpenChannel, PCNMessage},
    CkbConfig, Command, Event,
};

pub const PCN_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

pub struct NetworkControllerActor {}

pub struct NetworkStateNew {
    peers: HashMap<PeerId, PeerInfo>,
    control: ServiceAsyncControl,
}

#[rasync_trait]
impl Actor for NetworkControllerActor {
    // An actor has a message type
    type Msg = Command;
    // and (optionally) internal state
    type State = NetworkStateNew;
    // Startup arguments for actor initialization
    type Arguments = (mpsc::Sender<Event>, CkbConfig, TaskTracker);

    // Initially we need to create our state, and potentially
    // start some internal processing (by posting a message for
    // example)
    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        (event_sender, config, tracker): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
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
        Ok(NetworkStateNew {
            peers: HashMap::new(),
            control,
        })
    }

    // This is our main message handler
    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        command: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("Processing command {:?}", command);

        match command {
            Command::SendPcnMessage(PCNMessageWithPeerId { peer_id, message }) => {
                match state
                    .peers
                    .get(&peer_id)
                    .and_then(|p| p.sessions.iter().next())
                    .cloned()
                {
                    Some(session_id) => {
                        let result = state
                            .control
                            .send_message_to(
                                session_id,
                                PCN_PROTOCOL_ID,
                                message.to_molecule_bytes(),
                            )
                            .await;
                        if let Err(err) = result {
                            error!(
                                "Sending message to session {:?} failed: {}",
                                &session_id, err
                            );
                            return Ok(());
                        }
                        info!("Message send to peer {:?}", &peer_id);
                    }
                    None => {
                        error!("Session for peer {:?} not found", &peer_id);
                    }
                }
            }

            Command::ConnectPeer(addr) => {
                // TODO: It is more than just dialing a peer. We need to exchange capabilities of the peer,
                // e.g. whether the peer support some specific feature.
                // TODO: If we are already connected to the peer, skip connecting.
                debug!("Dialing {}", &addr);
                let result = state.control.dial(addr.clone(), TargetProtocol::All).await;
                if let Err(err) = result {
                    error!("Dialing {} failed: {}", &addr, err);
                }
            }

            Command::ControlPcnChannel(c) => match c {
                ChannelCommand::OpenChannel(open_channel) => {}
            },
        }
        Ok(())
    }
}

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

    pub fn handle_pcnmessage(
        &self,
        peer_id: PeerId,
        peer: &mut PeerInfo,
        msg: PCNMessage,
    ) -> ProcessingChannelResult {
        match msg {
            PCNMessage::TestMessage(test) => {
                debug!("Test message {:?}", test);
                Ok(())
            }
            PCNMessage::OpenChannel(open_channel) => Ok(()),

            PCNMessage::AcceptChannel(accpet_channel) => {
                debug!("Accepting channel {:?}", &accpet_channel);
                let channel = match peer.channels.get_mut(&accpet_channel.channel_id) {
                    Some(channel) => channel,
                    None => {
                        return Err(ProcessingChannelError::InvalidParameter(format!(
                            "Trying to accept channel {:?} that does not exist",
                            &accpet_channel.channel_id
                        )));
                    }
                };
                channel.step(ChannelEvent::AcceptChannel(accpet_channel))
            }

            _ => {
                error!("Message handling for {:?} unimplemented", msg);
                Ok(())
            }
        }
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

        let msg = unwrap_or_return!(PCNMessage::from_molecule_slice(&data), "parse message");
        let peer_id = match context.session.remote_pubkey.clone().map(Into::into) {
            Some(peer_id) => peer_id,
            None => {
                warn!("Received message from a peer without public key");
                return;
            }
        };
        let mut peer_state = self.state.peers.lock().await;
        let peer = match peer_state.get_mut(&peer_id) {
            Some(peer) => peer,
            None => {
                warn!("Trying to send message to unknown peer {:?}", peer_id);
                return;
            }
        };
        if let Err(err) = self.handle_pcnmessage(peer_id, peer, msg) {
            error!("Error while processing message: {:?}", err);
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

#[derive(Debug, Default)]
struct PeerInfo {
    sessions: HashSet<SessionId>,
    channels: HashMap<Hash256, ChannelActorState>,
}

struct RootActor;

#[rasync_trait]
impl Actor for RootActor {
    type Msg = String;
    type State = ();
    type Arguments = CancellationToken;

    /// Spawn a thread that waits for token to be cancelled,
    /// after that kill all sub actors.
    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        token: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        tokio::spawn(async move {
            token.cancelled().await;
            myself.stop(None);
        });
        Ok(())
    }
}

pub async fn start_ckb(
    config: CkbConfig,
    mut command_receiver: mpsc::Receiver<Command>,
    command_sender: mpsc::Sender<Command>,
    event_sender: mpsc::Sender<Event>,
    token: CancellationToken,
    tracker: TaskTracker,
) {
    let (root_actor, _) = Actor::spawn(Some("root actor".to_string()), RootActor, token.clone())
        .await
        .expect("Failed to start root actor");

    let (actor, _handle) = Actor::spawn_linked(
        Some("network controller".to_string()),
        NetworkControllerActor {},
        (event_sender, config, tracker.clone()),
        root_actor.get_cell(),
    )
    .await
    .expect("Failed to start network controller actor");

    tracker.spawn(async move {
        loop {
            select! {
                command = (&mut command_receiver).recv() => {
                    match command {
                        Some(command) => actor.send_message(command).expect("network controller alive"),
                        None => {
                            info!("Command reciever exited, exiting forwarding program");
                            return;
                        }
                    }
                },
                _ = token.cancelled() => {
                    return;
                }
            }
        }
    });
}
