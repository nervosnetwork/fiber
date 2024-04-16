use log::{debug, error, info, warn};
use ractor::{async_trait as rasync_trait, Actor, ActorProcessingErr, ActorRef};
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr, FromInto};
use std::{collections::HashMap, str};
use tentacle::{context::SessionContext, multiaddr::Multiaddr, secio::PeerId, SessionId};

use super::{channel::ChannelCommand, types::PCNMessage};

use tentacle::{
    async_trait,
    builder::{MetaBuilder, ServiceBuilder},
    bytes::Bytes,
    context::{ProtocolContext, ProtocolContextMutRef, ServiceContext},
    service::{
        ProtocolHandle, ProtocolMeta, ServiceAsyncControl, ServiceError, ServiceEvent,
        TargetProtocol,
    },
    traits::{ServiceHandle, ServiceProtocol},
    ProtocolId,
};
use tokio::select;
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    ckb::{
        channel::{
            ChannelActor, ChannelInitializationParameter, COUNTERPARTY_INITIAL_COMMITMENT_NUMBER,
            DEFAULT_COMMITMENT_FEE_RATE, DEFAULT_FEE_RATE, DEFAULT_MAX_ACCEPT_TLCS,
            DEFAULT_MAX_TLC_VALUE_IN_FLIGHT, DEFAULT_MIN_TLC_VALUE, DEFAULT_TO_SELF_DELAY,
            HOLDER_INITIAL_COMMITMENT_NUMBER,
        },
        peer::PeerActorMessage,
        types::AcceptChannel,
    },
    Config,
};

use crate::unwrap_or_return;

use super::{peer::PeerActor, CkbConfig, Event};

pub const PCN_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub enum NetworkActorCommand {
    /// Network commands
    ConnectPeer(Multiaddr),
    // For internal use and debugging only. Most of the messages requires some
    // changes to local state. Even if we can send a message to a peer, some
    // part of the local state is not changed.
    SendPcnMessage(PCNMessageWithPeerId),
    // Directly send a message to session
    SendPcnMessageToSession(PCNMessageWithSessionId),
    ControlPcnChannel(ChannelCommand),
}

impl NetworkActorMessage {
    pub fn new_event(event: NetworkActorEvent) -> Self {
        Self::Event(event)
    }

    pub fn new_command(command: NetworkActorCommand) -> Self {
        Self::Command(command)
    }
}

#[derive(Clone, Debug)]
pub enum NetworkActorEvent {
    /// Network events
    PeerConnected(PeerId, ActorRef<PeerActorMessage>),
    PeerDisconnected(PeerId, ActorRef<PeerActorMessage>),
}

#[derive(Clone, Debug)]
pub enum NetworkActorMessage {
    Command(NetworkActorCommand),
    Event(NetworkActorEvent),
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct PCNMessageWithPeerId {
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    pub message: PCNMessage,
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct PCNMessageWithSessionId {
    #[serde_as(as = "FromInto<usize>")]
    pub session_id: SessionId,
    pub message: PCNMessage,
}

pub struct NetworkActor {}

pub struct NetworkActorState {
    // This immutable attribute is placed here because we need to create it in
    // the pre_start function.
    control: ServiceAsyncControl,
    peers: HashMap<PeerId, ActorRef<PeerActorMessage>>,
}

impl NetworkActorState {
    /// Get or create a peer actor.
    pub async fn get_or_create_peer(
        id: PeerId,
        control: &ActorRef<NetworkActorMessage>,
    ) -> ActorRef<PeerActorMessage> {
        PeerActor::get_or_create(Some(id), &control)
            .await
            .expect("must create peer actor if peer id passed")
    }
}

#[rasync_trait]
impl Actor for NetworkActor {
    type Msg = NetworkActorMessage;
    type State = NetworkActorState;
    type Arguments = (mpsc::Sender<Event>, CkbConfig, TaskTracker);

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (event_sender, config, tracker): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let kp = config
            .read_or_generate_secret_key()
            .expect("read or generate secret key");
        let pk = kp.public_key();
        let handle = Handle::new(event_sender, myself);
        let mut service = ServiceBuilder::default()
            .insert_protocol(handle.clone().create_meta(PCN_PROTOCOL_ID))
            .key_pair(kp)
            .build(handle);
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
        Ok(NetworkActorState {
            peers: HashMap::new(),
            control,
        })
    }

    // This is our main message handler
    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("Processing network message {:?}", message);

        match message {
            NetworkActorMessage::Event(event) => match event {
                NetworkActorEvent::PeerConnected(peer_id, actor) => {
                    if state.peers.contains_key(&peer_id) {
                        warn!("Duplicated peer connected event. Are we connecting here? The connection reestablishment processing is not implemented");
                    } else {
                        debug!("Saved actor for peer {:?} to network actor", peer_id);
                        state.peers.insert(peer_id, actor);
                    }
                }
                NetworkActorEvent::PeerDisconnected(peer_id, actor) => {
                    match state.peers.remove(&peer_id) {
                        Some(_) => {
                            debug!("Removed actor for peer {:?} from network actor", peer_id);
                        }
                        None => {
                            error!("Peer {:?} not found", &peer_id);
                        }
                    }
                }
            },
            NetworkActorMessage::Command(command) => match command {
                NetworkActorCommand::SendPcnMessageToSession(PCNMessageWithSessionId {
                    session_id,
                    message,
                }) => {
                    let result = state
                        .control
                        .send_message_to(session_id, PCN_PROTOCOL_ID, message.to_molecule_bytes())
                        .await;
                    if let Err(err) = result {
                        error!(
                            "Sending message to session {:?} failed: {}",
                            &session_id, err
                        );
                        return Ok(());
                    }
                    debug!("Message send to session {:?}", &session_id);
                }

                NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId { peer_id, message }) => {
                    match state.peers.get(&peer_id) {
                        Some(actor) => {
                            actor
                                .send_message(PeerActorMessage::SendMessage(message))
                                .expect("peer actor alive");
                        }
                        None => {
                            error!("Peer {:?} not found", &peer_id);
                        }
                    }
                }

                NetworkActorCommand::ConnectPeer(addr) => {
                    // TODO: It is more than just dialing a peer. We need to exchange capabilities of the peer,
                    // e.g. whether the peer support some specific feature.
                    // TODO: If we are already connected to the peer, skip connecting.
                    debug!("Dialing {}", &addr);
                    let result = state.control.dial(addr.clone(), TargetProtocol::All).await;
                    if let Err(err) = result {
                        error!("Dialing {} failed: {}", &addr, err);
                    }
                }

                NetworkActorCommand::ControlPcnChannel(c) => match c {
                    ChannelCommand::OpenChannel(open_channel) => {
                        let peer_actor = state.peers.get(&open_channel.peer_id).cloned();
                        match peer_actor {
                            None => {
                                error!("Peer {:?} not found", &open_channel.peer_id);
                                return Ok(());
                            }
                            Some(peer_actor) => {
                                if let Err(err) = Actor::spawn(
                                    Some("channel".to_string()),
                                    ChannelActor::new(myself, peer_actor),
                                    ChannelInitializationParameter::OpenChannelCommand(
                                        open_channel,
                                    ),
                                )
                                .await
                                {
                                    error!("Failed to start channel actor: {}", err);
                                }
                            }
                        }
                    }
                },
            },
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct Handle {
    event_sender: mpsc::Sender<Event>,
    actor: ActorRef<NetworkActorMessage>,
}

impl Handle {
    fn new(event_sender: mpsc::Sender<Event>, actor: ActorRef<NetworkActorMessage>) -> Self {
        Self {
            event_sender,
            actor,
        }
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
        let _ = self.event_sender.send(event).await;
    }
}

#[async_trait]
impl ServiceProtocol for Handle {
    async fn init(&mut self, _context: &mut ProtocolContext) {}

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, version: &str) {
        let session = context.session;
        info!(
            "proto id [{}] open on session [{}], address: [{}], type: [{:?}], version: {}",
            context.proto_id, session.id, session.address, session.ty, version
        );
        self.send_event(Event::PeerConnected(context.session.address.clone()))
            .await;

        if let Some(actor) = PeerActor::get_or_create(
            context.session.remote_pubkey.clone().map(PeerId::from),
            &self.actor,
        )
        .await
        {
            actor
                .send_message(PeerActorMessage::Connected(context.session.clone()))
                .expect("peer actor alive");
        };
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        info!(
            "proto id [{}] close on session [{}]",
            context.proto_id, context.session.id
        );
        self.send_event(Event::PeerDisConnected(context.session.address.clone()))
            .await;

        if let Some(actor) = PeerActor::get_or_create(
            context.session.remote_pubkey.clone().map(PeerId::from),
            &self.actor,
        )
        .await
        {
            actor
                .send_message(PeerActorMessage::Disconnected(context.session.clone()))
                .expect("peer actor alive");
        };
    }

    async fn received(&mut self, context: ProtocolContextMutRef<'_>, data: Bytes) {
        info!(
            "received from [{}]: proto [{}] data {:?}",
            context.session.id,
            context.proto_id,
            hex::encode(data.as_ref()),
        );

        let msg = unwrap_or_return!(PCNMessage::from_molecule_slice(&data), "parse message");
        if let Some(actor) = PeerActor::get_or_create(
            context.session.remote_pubkey.clone().map(PeerId::from),
            &self.actor,
        )
        .await
        {
            actor
                .send_message(PeerActorMessage::Message(context.session.clone(), msg))
                .expect("peer actor alive");
        };
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}

#[async_trait]
impl ServiceHandle for Handle {
    async fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        self.send_event(Event::ServiceError(error)).await;
    }
    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        self.send_event(Event::ServiceEvent(event)).await;
    }
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
    mut command_receiver: mpsc::Receiver<NetworkActorCommand>,
    command_sender: mpsc::Sender<NetworkActorCommand>,
    event_sender: mpsc::Sender<Event>,
    token: CancellationToken,
    tracker: TaskTracker,
) {
    let (root_actor, _) = Actor::spawn(Some("root actor".to_string()), RootActor, token.clone())
        .await
        .expect("Failed to start root actor");

    let (actor, _handle) = Actor::spawn_linked(
        Some("network controller".to_string()),
        NetworkActor {},
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
                        Some(command) => actor.send_message(NetworkActorMessage::new_command(command)).expect("network controller alive"),
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
