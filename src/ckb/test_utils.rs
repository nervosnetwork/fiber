use std::time::Duration;

use log::debug;
use ractor::{async_trait, Actor, ActorProcessingErr, ActorRef};
use tempdir::TempDir;
use tentacle::{secio::PeerId, service::ServiceEvent};
use tokio::{select, sync::mpsc, time::sleep};

use crate::{
    actors::{RootActor, RootActorMessage},
    tasks::{new_tokio_cancellation_token, new_tokio_task_tracker},
    CkbConfig, NetworkServiceEvent,
};

use super::{
    channel::{ChannelActor, ChannelActorMessage, ChannelInitializationParameter},
    NetworkActor, NetworkActorMessage,
};

pub struct NetworkNode {
    /// The base directory of the node, will be deleted after this struct dropped.
    pub base_dir: TempDir,
    pub port: u16,
    pub network_actor: ActorRef<NetworkActorMessage>,
    pub peer_id: PeerId,
    pub event_emitter: mpsc::Receiver<NetworkServiceEvent>,
}

impl NetworkNode {
    pub async fn new() -> Self {
        let base_dir = TempDir::new("ckb-pcn-node").expect("create temp dir");
        let ckb_config = CkbConfig {
            base_dir: Some(base_dir.into_path()),
            ..Default::default()
        };

        let root = get_mock_root_actor().await;
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

        let listening_port = 0;
        loop {
            select! {
                Some(NetworkServiceEvent::NetworkStarted(peer_id, multiaddr)) = event_receiver.recv() => {
                    listening_port = multiaddr.port();
                    break;
                }
                _ = sleep(Duration::from_secs(5)) => {
                    panic!("Failed to start network actor");
                }
            }
        }

        Self {
            base_dir,
            port: listening_port,
            network_actor,
            peer_id,
            event_emitter: event_receiver,
        }
    }
}

pub async fn get_mock_root_actor() -> ActorRef<RootActorMessage> {
    Actor::spawn(
        Some("mock root actor".to_string()),
        RootActor {},
        (new_tokio_task_tracker(), new_tokio_cancellation_token()),
    )
    .await
    .expect("start mock root actor")
    .0
}

pub fn get_sample_ckb_config() -> CkbConfig {
    CkbConfig {
        base_dir: Some("/tmp/ckb-tmp".into()),
        ..Default::default()
    }
}

pub async fn create_tmp_network_actor() -> ActorRef<NetworkActorMessage> {
    let root = get_mock_root_actor().await;
    let (event_sender, mut event_receiver) = mpsc::channel(10000);

    new_tokio_task_tracker().spawn(async move {
        let token = new_tokio_cancellation_token();
        loop {
            select! {
                event = event_receiver.recv() => {
                    match event {
                        None => {
                            debug!("Event receiver completed, event processing service");
                            break;
                        }
                        Some(event) => {
                            debug!("Received event: {:?}", event);
                        }
                    }
                }
                _ = token.cancelled() => {
                    debug!("Cancellation received, event processing service");
                    break;
                }
            }
        }
        debug!("Event processing service exited");
    });

    Actor::spawn_linked(
        Some("network actor".to_string()),
        NetworkActor::new(event_sender),
        (get_sample_ckb_config(), new_tokio_task_tracker()),
        root.get_cell(),
    )
    .await
    .expect("start network actor")
    .0
}

async fn create_initiator_acceptor_pair(
    network: ActorRef<NetworkActorMessage>,
) -> ActorRef<ChannelActorMessage> {
    let acceptor_peer_id = PeerId::random();
    let network = create_tmp_network_actor().await;

    let initator = Actor::spawn_linked(
        Some("initator".to_string()),
        handler,
        startup_args,
        network.get_cell(),
    )
    .await
    .expect("Start initiator actor");

    let acceptor = Actor::spawn_linked(
        Some("channel".to_string()),
        ChannelActor::new(acceptor_peer_id.clone(), network),
        ChannelInitializationParameter::OpenChannelMessage(acceptor_peer_id.clone(), 0, o),
        network.get_cell(),
    )
    .await
    .expect("Start channel actor");
    acceptor.0
}
