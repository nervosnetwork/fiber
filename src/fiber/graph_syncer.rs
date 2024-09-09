//! This is the main module for the graph syncer. It is responsible for
//! syncing the graph with one specific peer.

use std::{cmp::max, u64};

use ractor::{async_trait as rasync_trait, call, Actor, ActorProcessingErr, ActorRef};
use tentacle::secio::PeerId;
use tracing::{debug, error};

use super::{
    network::GraphSyncerExitStatus, NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
    ASSUME_NETWORK_ACTOR_ALIVE,
};

// We assume all the channels with funding trsaction block number
// < latest height - ASSUME_MAX_CHANNEL_HEIGHT_GAP are already synced.
const ASSUME_MAX_CHANNEL_HEIGHT_GAP: u64 = 1000;

// We assume all the messages with timestamp <
// latest timestamp - ASSUME_MAX_MESSAGE_TIMESTAMP_GAP are already synced.
const ASSUME_MAX_MESSAGE_TIMESTAMP_GAP: u64 = 1000;

pub enum GraphSyncerMessage {
    PeerDisConnected,
    // The u64 is the starting height of the channels we want to sync.
    // The ending height is left to the syncer actor to decide.
    GetChannels(u64),
    // The u64 is the starting time of the messages we want to sync.
    // The ending time is left to the syncer actor to decide.I
    GetBroadcastMessages(u64),
}

pub struct GraphSyncerState {
    synced_height: u64,
    synced_time: u64,
}

pub struct GraphSyncer {
    network: ActorRef<NetworkActorMessage>,
    peer_id: PeerId,
    // We will only sync channels within this block range.
    starting_height: u64,
    ending_height: u64,
    // We will only sync messages within this time range.
    starting_time: u64,
    ending_time: u64,
}

impl GraphSyncer {
    pub fn new(
        network: ActorRef<NetworkActorMessage>,
        peer_id: PeerId,
        starting_height: u64,
        starting_time: u64,
    ) -> Self {
        let now = std::time::UNIX_EPOCH.elapsed().unwrap().as_millis() as u64;
        Self {
            network,
            peer_id,
            starting_height,
            // TODO: should use the actual current height.
            ending_height: u64::MAX,
            starting_time,
            ending_time: now,
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
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }
}

#[rasync_trait]
impl Actor for GraphSyncer {
    type Msg = GraphSyncerMessage;
    type State = GraphSyncerState;
    type Arguments = ();

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let starting_height = if self.starting_height < ASSUME_MAX_CHANNEL_HEIGHT_GAP {
            0
        } else {
            self.starting_height - ASSUME_MAX_CHANNEL_HEIGHT_GAP
        };
        myself.send_message(GraphSyncerMessage::GetChannels(starting_height))?;
        Ok(Self::State {
            synced_height: self.starting_height,
            synced_time: self.starting_time,
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            GraphSyncerMessage::PeerDisConnected => {
                self.tell_network_we_want_to_exit(GraphSyncerExitStatus::Failed);
            }
            GraphSyncerMessage::GetChannels(starting_height) => {
                if starting_height > self.ending_height {
                    panic!("Starting height to high (starting height {}, ending height {}), should have exited syncing earlier", starting_height, self.ending_height);
                }
                const STEP: u64 = 10;
                let ending_height = max(starting_height + STEP, self.ending_height);
                let request = |rpc_reply| {
                    NetworkActorMessage::new_command(
                        NetworkActorCommand::GetAndProcessChannelsWithinBlockRangeFromPeer(
                            (self.peer_id.clone(), starting_height, ending_height),
                            rpc_reply,
                        ),
                    )
                };
                match call!(self.network, request).expect(ASSUME_NETWORK_ACTOR_ALIVE) {
                    Ok(_) => {
                        debug!("Get channels from peer successfully.");
                        if self.ending_height == ending_height {
                            debug!("Starting get broadcast messages from peer after getting channels finished");
                            myself.send_message(GraphSyncerMessage::GetBroadcastMessages(
                                self.starting_time,
                            ))?;
                        } else {
                            myself.send_message(GraphSyncerMessage::GetChannels(ending_height))?;
                        }
                    }
                    Err(e) => {
                        error!("Failed to get channels from peer: {:?}", e);
                        self.tell_network_we_want_to_exit(GraphSyncerExitStatus::Failed);
                    }
                }
            }
            GraphSyncerMessage::GetBroadcastMessages(starting_time) => {
                if starting_time > self.ending_time {
                    panic!("Starting time to high (starting time {}, ending time {}), should have exited syncing earlier", starting_time, self.ending_time);
                }
                const STEP: u64 = 10;
                let ending_time = max(starting_time + STEP, self.ending_time);
                let request = |rpc_reply| {
                    NetworkActorMessage::new_command(
                        NetworkActorCommand::GetAndProcessBroadcastMessagesWithinTimeRangeFromPeer(
                            (self.peer_id.clone(), starting_time, ending_time),
                            rpc_reply,
                        ),
                    )
                };
                match call!(self.network, request).expect(ASSUME_NETWORK_ACTOR_ALIVE) {
                    Ok(_) => {
                        debug!("Get broadcast messages from peer successfully.");
                        if self.ending_time == ending_time {
                            debug!("Graph syncer finished syncing with peer.");
                            self.tell_network_we_want_to_exit(GraphSyncerExitStatus::Succeeded);
                        } else {
                            myself.send_message(GraphSyncerMessage::GetBroadcastMessages(
                                ending_time,
                            ))?;
                        }
                    }
                    Err(e) => {
                        error!("Failed to get broadcast messages from peer: {:?}", e);
                        self.tell_network_we_want_to_exit(GraphSyncerExitStatus::Failed);
                    }
                }
            }
        }
        Ok(())
    }
}
