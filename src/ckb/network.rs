use ckb_types::packed::{OutPoint, Transaction};
use log::{debug, error, info, warn};
use ractor::{async_trait as rasync_trait, Actor, ActorCell, ActorProcessingErr, ActorRef};
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr, FromInto};
use std::collections::HashSet;
use std::{collections::HashMap, str};

use tentacle::{
    async_trait,
    builder::{MetaBuilder, ServiceBuilder},
    bytes::Bytes,
    context::SessionContext,
    context::{ProtocolContext, ProtocolContextMutRef, ServiceContext},
    multiaddr::Multiaddr,
    secio::PeerId,
    service::{
        ProtocolHandle, ProtocolMeta, ServiceAsyncControl, ServiceError, ServiceEvent,
        TargetProtocol,
    },
    traits::{ServiceHandle, ServiceProtocol},
    ProtocolId, SessionId,
};

use tokio::sync::mpsc;
use tokio_util::task::TaskTracker;

use super::channel::{
    ChannelActorMessage, ChannelCommandWithId, ChannelEvent, ProcessingChannelError,
};
use super::types::{Hash256, OpenChannel};
use super::{
    channel::{ChannelActor, ChannelCommand, ChannelInitializationParameter},
    types::PCNMessage,
    CkbConfig,
};

use crate::{unwrap_or_return, Error};

pub const PCN_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

#[serde_as]
#[derive(Debug, Deserialize)]
pub enum NetworkActorCommand {
    /// Network commands
    ConnectPeer(Multiaddr),
    // For internal use and debugging only. Most of the messages requires some
    // changes to local state. Even if we can send a message to a peer, some
    // part of the local state is not changed.
    SendPcnMessage(PCNMessageWithPeerId),
    // Directly send a message to session
    SendPcnMessageToSession(PCNMessageWithSessionId),
    // Open channel to a peer.
    OpenChannel(OpenChannelCommand),
    // Send a command to a channel.
    ControlPcnChannel(ChannelCommandWithId),
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct OpenChannelCommand {
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    pub total_value: u128,
}

impl NetworkActorMessage {
    pub fn new_event(event: NetworkActorEvent) -> Self {
        Self::Event(event)
    }

    pub fn new_command(command: NetworkActorCommand) -> Self {
        Self::Command(command, None)
    }
}

#[derive(Debug)]
pub enum NetworkServiceEvent {
    ServiceError(ServiceError),
    ServiceEvent(ServiceEvent),
    NetworkStarted(PeerId, Multiaddr),
    PeerConnected(PeerId, Multiaddr),
    PeerDisConnected(PeerId, Multiaddr),
    ChannelCreated(PeerId, Hash256),
    ChannelReady(PeerId, Hash256),
}

/// Events that can be sent to the network actor. Except for NetworkServiceEvent,
/// all events are processed by the network actor.
#[derive(Debug)]
pub enum NetworkActorEvent {
    /// Network eventss to be processed by this actor.
    PeerConnected(PeerId, SessionContext),
    PeerDisconnected(PeerId, SessionContext),
    PeerMessage(PeerId, SessionContext, PCNMessage),

    /// Channel related events.
    /// A new channel is created and the peer id and actor reference is given here.
    ChannelCreated(Hash256, PeerId, ActorRef<ChannelActorMessage>),
    /// A channel has been accepted. The two Hash256 are respectively newly agreed channel id and temp channel id.
    ChannelAccepted(Hash256, Hash256),
    /// A channel is ready to use.
    ChannelReady(Hash256, PeerId),

    /// Both parties are now able to broadcast a valid funding transaction.
    FundingTransactionPending(Transaction, OutPoint, Hash256),

    /// A funding transaction has been confirmed.
    FundingTransactionConfirmed(OutPoint),

    /// Network service events to be sent to outside observers.
    /// We will not do any processing on these events.
    NetworkServiceEvent(NetworkServiceEvent),
}

#[derive(Debug)]
pub enum NetworkActorMessage {
    Command(
        NetworkActorCommand,
        // TODO: we may need to refine the following type according to each commands.
        Option<mpsc::Sender<crate::Result<()>>>,
    ),
    Event(NetworkActorEvent),
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct PCNMessageWithPeerId {
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    pub message: PCNMessage,
}

impl PCNMessageWithPeerId {
    pub fn new(peer_id: PeerId, message: PCNMessage) -> Self {
        Self { peer_id, message }
    }
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct PCNMessageWithSessionId {
    #[serde_as(as = "FromInto<usize>")]
    pub session_id: SessionId,
    pub message: PCNMessage,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PCNMessageWithChannelId {
    pub channel_id: Hash256,
    pub message: PCNMessage,
}

pub struct NetworkActor {
    // An event emitter to notify ourside observers.
    event_sender: mpsc::Sender<NetworkServiceEvent>,
}

impl NetworkActor {
    pub fn new(event_sender: mpsc::Sender<NetworkServiceEvent>) -> Self {
        Self { event_sender }
    }

    pub async fn on_service_event(&self, event: NetworkServiceEvent) {
        let _ = self.event_sender.send(event).await;
    }

    pub async fn handle_peer_message(
        &self,
        state: &mut NetworkActorState,
        peer_id: PeerId,
        session: SessionContext,
        message: PCNMessage,
    ) -> crate::Result<()> {
        debug!(
            "Received message from peer {:?} on session {:?}: {:?}",
            &peer_id, &session.id, &message
        );

        match message {
            PCNMessage::OpenChannel(open_channel) => {
                state.create_inbound_channel(peer_id, open_channel).await?;
            }
            PCNMessage::TestMessage(test) => {
                debug!("Test message {:?}", test);
            }

            _ => match state.channels.get(&message.get_channel_id()) {
                None => {
                    return Err(Error::ChannelNotFound(message.get_channel_id()));
                }
                Some(c) => {
                    c.send_message(ChannelActorMessage::PeerMessage(message))
                        .expect("channel actor alive");
                }
            },
        };
        Ok(())
    }

    pub async fn handle_command(
        &self,
        state: &mut NetworkActorState,
        command: NetworkActorCommand,
    ) -> crate::Result<()> {
        match command {
            NetworkActorCommand::SendPcnMessageToSession(PCNMessageWithSessionId {
                session_id,
                message,
            }) => {
                debug!(
                    "SendPcnMessageToSession command received: Sending message to session {:?}",
                    session_id
                );
                state.send_message_to_session(session_id, message).await?;
            }

            NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId { peer_id, message }) => {
                debug!(
                    "SendPcnMessage command received: sending message to peer {:?}",
                    &peer_id
                );
                state.send_message_to_peer(&peer_id, message).await?;
            }

            NetworkActorCommand::ConnectPeer(addr) => {
                // TODO: It is more than just dialing a peer. We need to exchange capabilities of the peer,
                // e.g. whether the peer support some specific feature.
                // TODO: If we are already connected to the peer, skip connecting.
                debug!("ConnectPeer command received, dialing {}", &addr);
                state
                    .control
                    .dial(addr.clone(), TargetProtocol::All)
                    .await?
                // TODO: note that the dial function does not return error immediately even if dial fails.
                // Tentacle sends an event by calling handle_error function instead, which
                // may receive errors like DialerError.
            }

            NetworkActorCommand::OpenChannel(open_channel) => {
                state.create_outbound_channel(open_channel).await?;
            }

            NetworkActorCommand::ControlPcnChannel(c) => {
                state
                    .send_command_to_channel(c.channel_id, c.command)
                    .await?
            }
        };
        Ok(())
    }
}

pub struct NetworkActorState {
    peer_id: PeerId,
    network: ActorRef<NetworkActorMessage>,
    // This immutable attribute is placed here because we need to create it in
    // the pre_start function.
    control: ServiceAsyncControl,
    peers: HashMap<PeerId, HashSet<SessionId>>,
    channels: HashMap<Hash256, ActorRef<ChannelActorMessage>>,
    // Channels in this hashmap are pending for funding transaction confirmation.
    pending_channels: HashMap<OutPoint, Hash256>,
}

impl NetworkActorState {
    pub async fn create_outbound_channel(
        &mut self,
        open_channel: OpenChannelCommand,
    ) -> Result<ActorRef<ChannelActorMessage>, ProcessingChannelError> {
        let network = self.network.clone();
        Ok(Actor::spawn_linked(
            None,
            ChannelActor::new(open_channel.peer_id.clone(), network.clone()),
            ChannelInitializationParameter::OpenChannelCommand(open_channel),
            network.clone().get_cell(),
        )
        .await?
        .0)
    }

    pub async fn create_inbound_channel(
        &mut self,
        peer_id: PeerId,
        open_channel: OpenChannel,
    ) -> Result<ActorRef<ChannelActorMessage>, ProcessingChannelError> {
        let network = self.network.clone();
        let id = open_channel.channel_id;
        if let Some(channel) = self.channels.get(&id) {
            warn!("A channel of id {:?} is already created, returning it", &id);
            return Ok(channel.clone());
        }
        let channel_user_id = self.channels.len();

        let channel = Actor::spawn_linked(
            None,
            ChannelActor::new(peer_id.clone(), network.clone()),
            ChannelInitializationParameter::OpenChannelMessage(
                peer_id.clone(),
                channel_user_id,
                open_channel,
            ),
            network.clone().get_cell(),
        )
        .await?
        .0;
        Ok(channel)
    }

    fn get_peer_session(&self, peer_id: &PeerId) -> Option<SessionId> {
        self.peers
            .get(peer_id)
            .and_then(|sessions| sessions.iter().next().cloned())
    }

    async fn send_message_to_session(
        &self,
        session_id: SessionId,
        message: PCNMessage,
    ) -> crate::Result<()> {
        self.control
            .send_message_to(session_id, PCN_PROTOCOL_ID, message.to_molecule_bytes())
            .await?;
        Ok(())
    }

    async fn send_message_to_peer(
        &self,
        peer_id: &PeerId,
        message: PCNMessage,
    ) -> crate::Result<()> {
        match self.get_peer_session(peer_id) {
            Some(session) => self.send_message_to_session(session, message).await,
            None => Err(Error::PeerNotFound(peer_id.clone())),
        }
    }

    async fn send_command_to_channel(
        &self,
        channel_id: Hash256,
        command: ChannelCommand,
    ) -> crate::Result<()> {
        match self.channels.get(&channel_id) {
            Some(actor) => {
                actor.send_message(ChannelActorMessage::Command(command))?;
                Ok(())
            }
            None => Err(Error::ChannelNotFound(channel_id)),
        }
    }

    fn on_peer_connected(&mut self, id: &PeerId, session: &SessionContext) {
        debug!("Peer connected: {:?}", &id);
        let sessions = self.peers.entry(id.clone()).or_default();
        sessions.insert(session.id);
    }

    fn on_peer_disconnected(&mut self, id: &PeerId, session: &SessionContext) {
        debug!("Peer disconnected: {:?}", &id);
        if let Some(sessions) = self.peers.get_mut(id) {
            sessions.remove(&session.id);
            if sessions.is_empty() {
                self.peers.remove(id);
            }
        }
    }

    fn on_channel_created(
        &mut self,
        id: Hash256,
        peer_id: &PeerId,
        actor: ActorRef<ChannelActorMessage>,
    ) {
        debug!("Channel to peer {:?} created: {:?}", &peer_id, &id);
        self.channels.insert(id, actor);
    }

    async fn on_funding_transaction_pending(
        &mut self,
        transaction: Transaction,
        outpoint: OutPoint,
        channel_id: Hash256,
    ) -> Result<(), ActorProcessingErr> {
        if let Some(old) = self.pending_channels.remove(&outpoint) {
            if old != channel_id {
                panic!("Trying to associate a new channel id {:?} with the same outpoint {:?} when old channel id is {:?}. Rejecting.", channel_id, outpoint, old);
            }
        }
        self.pending_channels.insert(outpoint.clone(), channel_id);
        // TODO: try to broadcast the transaction to the network.
        debug!(
            "Funding transaction (outpoint {:?}) for channel {:?} is now ready. We can broadcast transaction {:?} now.",
            &outpoint, &channel_id, &transaction
        );
        Ok(())
    }

    async fn on_funding_transaction_confirmed(
        &mut self,
        outpoint: OutPoint,
    ) -> Result<(), ActorProcessingErr> {
        let channel_id = self
            .pending_channels
            .remove(&outpoint)
            .expect("channel id exists");
        let channel = self.channels.get(&channel_id).expect("channel exists");
        channel
            .send_message(ChannelActorMessage::Event(
                ChannelEvent::FundingTransactionConfirmed(),
            ))
            .expect("channel actor alive");
        Ok(())
    }
}

#[rasync_trait]
impl Actor for NetworkActor {
    type Msg = NetworkActorMessage;
    type State = NetworkActorState;
    type Arguments = (CkbConfig, TaskTracker);

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (config, tracker): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let kp = config
            .read_or_generate_secret_key()
            .expect("read or generate secret key");
        let pk = kp.public_key();
        let handle = Handle::new(myself.clone());
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

        let my_peer_id: PeerId = PeerId::from(pk);
        info!(
            "Started listening tentacle on {}/p2p/{}",
            listen_addr,
            my_peer_id.to_base58()
        );

        let control = service.control().to_owned();

        myself
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::NetworkServiceEvent(NetworkServiceEvent::NetworkStarted(
                    my_peer_id.clone(),
                    listen_addr,
                )),
            ))
            .expect("network actor myself alive");

        tracker.spawn(async move {
            service.run().await;
            debug!("Tentacle service shutdown");
        });

        Ok(NetworkActorState {
            peer_id: my_peer_id,
            network: myself,
            control,
            peers: Default::default(),
            channels: Default::default(),
            pending_channels: Default::default(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("Network actor processing message {:?}", message);

        match message {
            NetworkActorMessage::Event(event) => match event {
                NetworkActorEvent::NetworkServiceEvent(e) => {
                    self.on_service_event(e).await;
                }
                NetworkActorEvent::PeerConnected(id, session) => {
                    state.on_peer_connected(&id, &session);
                    // Also send an PeerConnected event to outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::PeerConnected(id, session.address),
                            ),
                        ))
                        .expect("myself alive");
                }
                NetworkActorEvent::PeerDisconnected(id, session) => {
                    state.on_peer_disconnected(&id, &session);
                    // Also send an PeerDisconnected event to outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::PeerDisConnected(id, session.address),
                            ),
                        ))
                        .expect("myself alive");
                }
                NetworkActorEvent::ChannelCreated(channel_id, peer_id, actor) => {
                    state.on_channel_created(channel_id, &peer_id, actor);
                    // Also send an PeerDisconnected event to outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::ChannelCreated(peer_id, channel_id),
                            ),
                        ))
                        .expect("myself alive");
                }
                NetworkActorEvent::ChannelAccepted(new, old) => {
                    assert_ne!(new, old, "new and old channel id must be different");
                    let channel = state.channels.remove(&old).expect("channel exists");
                    state.channels.insert(new, channel);
                    debug!("Channel accepted: {:?} -> {:?}", old, new);
                }
                NetworkActorEvent::ChannelReady(channel_id, peer_id) => {
                    info!(
                        "Channel ({:?}) to peer {:?} is now ready",
                        channel_id, peer_id
                    );
                    // Also send an PeerDisconnected event to outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::ChannelReady(peer_id, channel_id),
                            ),
                        ))
                        .expect("myself alive");
                }
                NetworkActorEvent::PeerMessage(peer_id, session, message) => {
                    self.handle_peer_message(state, peer_id, session, message)
                        .await?
                }
                NetworkActorEvent::FundingTransactionPending(transaction, outpoint, channel_id) => {
                    debug!(
                        "Funding transaction pending for channel {:?}: {:?}",
                        channel_id, outpoint
                    );
                    state
                        .on_funding_transaction_pending(transaction, outpoint.clone(), channel_id)
                        .await?;

                    // TODO: Although we should really wait for the funding transaction to be confirmed,
                    // we have not integrated ckb rpc yet. So we will just send a
                    // FundingTransactionConfirmed notification here.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::FundingTransactionConfirmed(outpoint),
                        ))
                        .expect("network actor alive");
                }
                NetworkActorEvent::FundingTransactionConfirmed(outpoint) => {
                    state.on_funding_transaction_confirmed(outpoint).await?
                }
            },
            NetworkActorMessage::Command(command, sender) => {
                let result = self.handle_command(state, command).await;
                if let Some(sender) = sender {
                    sender.send(result).await.expect("receiver not closed");
                }
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let Err(err) = state.control.close().await {
            error!("Failed to close tentacle service: {}", err);
        }
        debug!("Network service for {:?} shutdown", state.peer_id);
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct Handle {
    actor: ActorRef<NetworkActorMessage>,
}

impl Handle {
    pub fn new(actor: ActorRef<NetworkActorMessage>) -> Self {
        Self { actor }
    }

    fn send_actor_message(&self, message: NetworkActorMessage) {
        // If we are closing the whole network service, we may have already stopped the network actor.
        // In that case the send_message will fail.
        // Ideally, we should close tentacle network service first, then stop the network actor.
        // But ractor provides only api for `post_stop` instead of `pre_stop`.
        let _ = self.actor.send_message(message);
    }

    fn emit_event(&self, event: NetworkServiceEvent) {
        self.send_actor_message(NetworkActorMessage::Event(
            NetworkActorEvent::NetworkServiceEvent(event),
        ));
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

        if let Some(peer_id) = context.session.remote_pubkey.clone().map(PeerId::from) {
            self.send_actor_message(NetworkActorMessage::new_event(
                NetworkActorEvent::PeerConnected(peer_id, context.session.clone()),
            ));
        } else {
            warn!("Peer connected without remote pubkey {:?}", context.session);
        }
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        info!(
            "proto id [{}] close on session [{}]",
            context.proto_id, context.session.id
        );

        if let Some(peer_id) = context.session.remote_pubkey.clone().map(PeerId::from) {
            self.send_actor_message(NetworkActorMessage::new_event(
                NetworkActorEvent::PeerDisconnected(peer_id, context.session.clone()),
            ));
        } else {
            warn!(
                "Peer disconnected without remote pubkey {:?}",
                context.session
            );
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
        if let Some(peer_id) = context.session.remote_pubkey.clone().map(PeerId::from) {
            self.send_actor_message(NetworkActorMessage::new_event(
                NetworkActorEvent::PeerMessage(peer_id, context.session.clone(), msg),
            ));
        } else {
            warn!(
                "Received message from a peer without remote pubkey {:?}",
                context.session
            );
        }
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}

#[async_trait]
impl ServiceHandle for Handle {
    async fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        self.emit_event(NetworkServiceEvent::ServiceError(error));
    }
    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        self.emit_event(NetworkServiceEvent::ServiceEvent(event));
    }
}

pub async fn start_ckb(
    config: CkbConfig,
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    tracker: TaskTracker,
    root_actor: ActorCell,
) -> ActorRef<NetworkActorMessage> {
    let (actor, _handle) = Actor::spawn_linked(
        Some("network actor".to_string()),
        NetworkActor { event_sender },
        (config, tracker),
        root_actor,
    )
    .await
    .expect("Failed to start network actor");

    actor
}
