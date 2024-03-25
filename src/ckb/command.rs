use serde::Deserialize;
use tentacle::multiaddr::Multiaddr;

#[derive(PartialEq, Eq, Clone, Debug, Deserialize)]
pub enum Command {
    Connect(Multiaddr),
}
