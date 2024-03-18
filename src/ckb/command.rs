use tentacle::multiaddr::Multiaddr;

#[derive(PartialEq, Eq, Clone, Debug)]
pub enum Command {
    Connect(Multiaddr),
}
