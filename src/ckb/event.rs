use serde::Deserialize;
use tentacle::multiaddr::Multiaddr;

#[derive(PartialEq, Eq, Clone, Debug, Deserialize)]
pub enum Event {
    PeerConnected(Multiaddr),
    PeerDisConnected(Multiaddr),
}
