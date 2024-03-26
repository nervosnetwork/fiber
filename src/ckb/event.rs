use tentacle::{
    multiaddr::Multiaddr,
    service::{ServiceError, ServiceEvent},
};

#[derive(Debug)]
pub enum Event {
    ServiceError(ServiceError),
    ServiceEvent(ServiceEvent),
    PeerConnected(Multiaddr),
    PeerDisConnected(Multiaddr),
}
