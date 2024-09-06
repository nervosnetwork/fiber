//! This is the main module for the graph syncer. It is responsible for
//! syncing the graph with one specific peer.

use std::time::SystemTime;

use ractor::{async_trait as rasync_trait, Actor, ActorProcessingErr, ActorRef};
use tentacle::secio::PeerId;

use super::{network::GraphSyncerExitStatus, NetworkActorEvent, NetworkActorMessage};

pub enum GraphSyncerMessage {
    PeerDisConnected,
}

pub struct GraphSyncerState {
    synced_height: u64,
    synced_time: SystemTime,
}

pub struct GraphSyncer {
    network: ActorRef<NetworkActorMessage>,
    peer_id: PeerId,
    // The block number we are syncing from.
    starting_height: u64,
    // The timestamp we started syncing.
    starting_time: SystemTime,
}

impl GraphSyncer {
    pub fn new(
        network: ActorRef<NetworkActorMessage>,
        peer_id: PeerId,
        starting_height: u64,
        starting_time: SystemTime,
    ) -> Self {
        Self {
            network,
            peer_id,
            starting_height,
            starting_time,
        }
    }
}

impl GraphSyncer {
    fn tell_network_we_want_to_exit(&self, status: GraphSyncerExitStatus) {
        let peer_id = self.peer_id.clone();
        self.network
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::GraphSyncerExited(peer_id, status),
            ))
            .expect("Network actor must be alive");
    }
}

#[rasync_trait]
impl Actor for GraphSyncer {
    type Msg = GraphSyncerMessage;
    type State = GraphSyncerState;
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(Self::State {
            synced_height: self.starting_height,
            synced_time: self.starting_time,
        })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            GraphSyncerMessage::PeerDisConnected => {
                self.tell_network_we_want_to_exit(GraphSyncerExitStatus::Failed);
            }
        }
        Ok(())
    }
}
