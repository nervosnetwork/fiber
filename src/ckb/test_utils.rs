use std::time::Duration;

use ractor::{Actor, ActorRef};
use tempfile::TempDir;
use tentacle::{multiaddr::MultiAddr, secio::PeerId};
use tokio::{
    select,
    sync::{mpsc, OnceCell},
    time::sleep,
};

use crate::{
    actors::{RootActor, RootActorMessage},
    tasks::{new_tokio_cancellation_token, new_tokio_task_tracker},
    CkbConfig, NetworkServiceEvent,
};

use super::{NetworkActor, NetworkActorMessage};

static ROOT_ACTOR: OnceCell<ActorRef<RootActorMessage>> = OnceCell::const_new();

pub async fn get_test_root_actor() -> ActorRef<RootActorMessage> {
    Actor::spawn(
        Some("test root actor".to_string()),
        RootActor {},
        (new_tokio_task_tracker(), new_tokio_cancellation_token()),
    )
    .await
    .expect("start test root actor")
    .0
}

#[derive(Debug)]
pub struct NetworkNode {
    /// The base directory of the node, will be deleted after this struct dropped.
    pub base_dir: TempDir,
    pub listening_addr: MultiAddr,
    pub network_actor: ActorRef<NetworkActorMessage>,
    pub peer_id: PeerId,
    pub event_emitter: mpsc::Receiver<NetworkServiceEvent>,
}

impl NetworkNode {
    pub async fn new() -> Self {
        let base_dir = TempDir::with_prefix("ckb-pcn-node-test").expect("create temp dir");
        let ckb_config = CkbConfig {
            base_dir: Some(base_dir.path().into()),
            ..Default::default()
        };

        let root = ROOT_ACTOR.get_or_init(get_test_root_actor).await.clone();
        let (event_sender, mut event_receiver) = mpsc::channel(10000);

        let network_actor = Actor::spawn_linked(
            Some("network actor".to_string()),
            NetworkActor::new(event_sender),
            (ckb_config, new_tokio_task_tracker()),
            root.get_cell(),
        )
        .await
        .expect("start network actor")
        .0;

        let (peer_id, listening_addr) = loop {
            select! {
                Some(NetworkServiceEvent::NetworkStarted(peer_id, multiaddr)) = event_receiver.recv() => {
                    break (peer_id, multiaddr);
                }
                _ = sleep(Duration::from_secs(5)) => {
                    panic!("Failed to start network actor");
                }
            }
        };

        Self {
            base_dir,
            listening_addr,
            network_actor,
            peer_id,
            event_emitter: event_receiver,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::NetworkNode;

    #[tokio::test]
    async fn test_start_network_node() {
        dbg!("start network node");
        let node = NetworkNode::new().await;
        dbg!("network node started", &node);
    }
}
