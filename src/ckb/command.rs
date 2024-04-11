use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};
use tentacle::{multiaddr::Multiaddr, secio::PeerId};

use super::{channel::ChannelCommand, types::PCNMessage};

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub enum Command {
    ConnectPeer(Multiaddr),
    // For internal use and debugging only. Most of the messages requires some
    // changes to local state. Even if we can send a message to a peer, some
    // part of the local state is not changed.
    SendPcnMessage(PCNMessageWithPeerId),
    ControlPcnChannel(ChannelCommand),
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct PCNMessageWithPeerId {
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    pub message: PCNMessage,
}
