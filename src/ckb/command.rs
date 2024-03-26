use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};
use tentacle::{multiaddr::Multiaddr, secio::PeerId};

use super::types::PCNMessage;

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub enum Command {
    ConnectPeer(Multiaddr),
    SendPcnMessage(#[serde_as(as = "DisplayFromStr")] PeerId, PCNMessage),
}
