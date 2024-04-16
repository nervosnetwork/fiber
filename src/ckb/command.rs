use serde::{Deserialize, Serialize};
use tentacle::multiaddr::Multiaddr;

use super::types::PCNMessage;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Connect(Multiaddr),
    SendMessage(Multiaddr, PCNMessage),
}
