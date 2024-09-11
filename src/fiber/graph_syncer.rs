//! This is the main module for the graph syncer. It is responsible for
//! syncing the graph with one specific peer.

use ractor::{async_trait as rasync_trait, call, Actor, ActorProcessingErr, ActorRef};
use tentacle::secio::PeerId;
use tracing::{debug, error};

use anyhow::anyhow;

use super::{
    network::GraphSyncerExitStatus, NetworkActorCommand, NetworkActorEvent, NetworkActorMessage,
    ASSUME_NETWORK_ACTOR_ALIVE,
};

// We assume all the channels with funding trsaction block number
// < latest height - ASSUME_MAX_CHANNEL_HEIGHT_GAP are already synced.
const ASSUME_MAX_CHANNEL_HEIGHT_GAP: u64 = 1000;

// We assume all the messages with timestamp <
// latest timestamp - ASSUME_MAX_MESSAGE_TIMESTAMP_GAP are already synced.
// The gap is currently set to 12 hours.
const ASSUME_MAX_MESSAGE_TIMESTAMP_GAP: u64 = 1000 * 3600 * 12;

#[derive(Debug)]
pub enum GraphSyncerMessage {
    PeerDisConnected,
    // The u64 is the starting height of the channels we want to sync.
    // The ending height is left to the syncer actor to decide.
    GetChannels(u64),
    // The u64 is the starting time of the messages we want to sync.
    // The ending time is left to the syncer actor to decide.I
    GetBroadcastMessages(u64),
}

pub struct GraphSyncerState {}

impl GraphSyncerState {
    fn new() -> Self {
        Self {}
    }
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
        ending_height: u64,
        starting_time: u64,
    ) -> Self {
        let now = std::time::UNIX_EPOCH.elapsed().unwrap().as_millis() as u64;
        Self {
            network,
            peer_id,
            starting_height,
            ending_height,
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
        if starting_height > self.ending_height {
            return Err(anyhow!(
                "Starting height for graph syncer too high (starting height {}, ending height {})",
                starting_height,
                self.ending_height
            )
            .into());
        }
        myself.send_message(GraphSyncerMessage::GetChannels(starting_height))?;
        Ok(Self::State::new())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("Graph syncer handling message {:?}", &message);
        match message {
            GraphSyncerMessage::PeerDisConnected => {
                self.tell_network_we_want_to_exit(GraphSyncerExitStatus::Failed);
            }
            GraphSyncerMessage::GetChannels(starting_height) => {
                if starting_height > self.ending_height {
                    panic!("Starting height to high (starting height {}, ending height {}), should have exited syncing earlier", starting_height, self.ending_height);
                }
                let request = |rpc_reply| {
                    NetworkActorMessage::new_command(
                        NetworkActorCommand::GetAndProcessChannelsWithinBlockRangeFromPeer(
                            (self.peer_id.clone(), starting_height, u64::MAX),
                            rpc_reply,
                        ),
                    )
                };
                match call!(self.network, request).expect(ASSUME_NETWORK_ACTOR_ALIVE) {
                    Ok((next_height, _is_finished)) => {
                        debug!("Get channels from peer successfully.");
                        if next_height > self.ending_height {
                            debug!("Starting get broadcast messages from peer after getting channels finished");
                            let starting_time =
                                if self.starting_time < ASSUME_MAX_MESSAGE_TIMESTAMP_GAP {
                                    0
                                } else {
                                    self.starting_time - ASSUME_MAX_MESSAGE_TIMESTAMP_GAP
                                };
                            myself.send_message(GraphSyncerMessage::GetBroadcastMessages(
                                starting_time,
                            ))?;
                        } else {
                            myself.send_message(GraphSyncerMessage::GetChannels(next_height))?;
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
                let request = |rpc_reply| {
                    NetworkActorMessage::new_command(
                        NetworkActorCommand::GetAndProcessBroadcastMessagesWithinTimeRangeFromPeer(
                            (self.peer_id.clone(), starting_time, u64::MAX),
                            rpc_reply,
                        ),
                    )
                };
                match call!(self.network, request).expect(ASSUME_NETWORK_ACTOR_ALIVE) {
                    Ok((next_time, _is_finished)) => {
                        debug!("Get broadcast messages from peer successfully.");
                        if next_time > self.ending_time {
                            debug!("Graph syncer finished syncing with peer.");
                            self.tell_network_we_want_to_exit(GraphSyncerExitStatus::Succeeded);
                        } else {
                            myself.send_message(GraphSyncerMessage::GetBroadcastMessages(
                                next_time,
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
