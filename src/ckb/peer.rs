use std::collections::{HashMap, HashSet};

use log::{debug, error, warn};
use ractor::{async_trait, Actor, ActorProcessingErr, ActorRef};
use tentacle::{context::SessionContext, secio::PeerId, SessionId};

use crate::ckb::{
    channel::{ChannelActor, ChannelInitializationParameter},
    network::PCNMessageWithSessionId,
};

use super::{
    channel::ChannelActorMessage,
    network::{NetworkActorEvent, NetworkActorMessage},
    types::{Hash256, PCNMessage},
    NetworkActorCommand,
};

#[derive(Debug, Default)]
pub struct PeerActorState {
    sessions: HashSet<SessionId>,
    channels: HashMap<Hash256, ActorRef<PCNMessage>>,
}

#[derive(Debug)]
pub enum PeerActorMessage {
    /// Events received from the underlying network stack.
    Connected(SessionContext),
    Disconnected(SessionContext),
    Message(SessionContext, PCNMessage),

    /// Events received from application layer.
    ChannelCreated(Hash256, ActorRef<ChannelActorMessage>),

    /// Commands to control the underlying network stack.
    SendMessage(PCNMessage),
}

pub struct PeerActor {
    pub id: Option<PeerId>,
    pub network: ActorRef<NetworkActorMessage>,
}

impl PeerActor {
    fn new(id: Option<PeerId>, network: ActorRef<NetworkActorMessage>) -> Self {
        Self { id, network }
    }

    pub async fn get_or_create(
        id: Option<PeerId>,
        network: &ActorRef<NetworkActorMessage>,
    ) -> Option<ActorRef<PeerActorMessage>> {
        Some(match id {
            None => return None,
            Some(p) => {
                let actor_name = get_peer_actor_name(&p);
                match ActorRef::where_is(actor_name.clone()) {
                    Some(a) => a,
                    None => {
                        Actor::spawn_linked(
                            Some(actor_name),
                            PeerActor::new(Some(p), network.clone()),
                            (),
                            network.clone().get_cell(),
                        )
                        .await
                        .expect("spawn peer actor")
                        .0
                    }
                }
            }
        })
    }
}

#[async_trait]
impl Actor for PeerActor {
    type Msg = PeerActorMessage;
    type State = PeerActorState;
    type Arguments = ();

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        if let Some(id) = (&self.id).clone() {
            self.network
                .send_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::PeerConnected(id, myself),
                ))
                .expect("network actor alive");
        }
        Ok(Default::default())
    }

    async fn post_stop(
        &self,
        myself: ActorRef<Self::Msg>,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let Some(id) = (&self.id).clone() {
            self.network
                .send_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::PeerDisconnected(id, myself),
                ))
                .expect("network actor alive");
        }
        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("Processing command {:?}", msg);
        match msg {
            PeerActorMessage::Connected(s) => state.sessions.extend(&[s.id]),
            PeerActorMessage::Disconnected(s) => {
                state.sessions.remove(&s.id);
            }
            PeerActorMessage::Message(_session, message) => match message {
                PCNMessage::OpenChannel(o) => {
                    let id = o.channel_id;
                    if state.channels.contains_key(&id) {
                        error!("Received duplicated open channel request");
                    }
                    let channel_user_id = state.channels.len();

                    if let Err(err) = Actor::spawn_linked(
                        Some("channel".to_string()),
                        ChannelActor::new(self.network.clone(), myself.clone()),
                        ChannelInitializationParameter::OpenChannel(
                            self.id.clone().expect("peer id must exist"),
                            channel_user_id,
                            o,
                        ),
                        self.network.clone().get_cell(),
                    )
                    .await
                    {
                        error!("Failed to create channel actor: {:?}", err);
                    }
                }

                PCNMessage::TestMessage(test) => {
                    debug!("Test message {:?}", test);
                }

                PCNMessage::AcceptChannel(m) => match state.channels.remove(&m.channel_id) {
                    None => {
                        warn!("Received an AcceptChannel message without saved correponding channale {:?}", m.channel_id);
                    }
                    Some(c) => c
                        .send_message(PCNMessage::AcceptChannel(m))
                        .expect("channel actor alive"),
                },

                _ => {
                    error!("Message handling for {:?} unimplemented", message);
                }
            },
            PeerActorMessage::SendMessage(message) => match state.sessions.iter().next() {
                Some(session_id) => self
                    .network
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessageToSession(PCNMessageWithSessionId {
                            session_id: *session_id,
                            message,
                        }),
                    ))
                    .expect("network actor alive"),
                None => {
                    error!("Session for peer {:?} not found", &self.id);
                }
            },

            PeerActorMessage::ChannelCreated(id, actor) => {
                if state.channels.contains_key(&id) {
                    error!("Received duplicated channel creation request");
                } else {
                    debug!("Channel created with id {:?}", id);
                    state.channels.insert(id, actor);
                }
            }
        }
        Ok(())
    }
}

fn get_peer_actor_name(peer_id: &PeerId) -> String {
    format!("peer {}", peer_id.to_base58())
}
