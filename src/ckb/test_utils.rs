use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    mem::ManuallyDrop,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use ckb_types::core::TransactionView;

use ractor::{Actor, ActorRef};

use tempfile::TempDir as OldTempDir;
use tentacle::{multiaddr::MultiAddr, secio::PeerId};
use tokio::{
    select,
    sync::{mpsc, OnceCell},
    time::sleep,
};

use crate::{
    actors::{RootActor, RootActorMessage},
    ckb_chain::{submit_tx, trace_tx, CkbChainMessage, MockChainActor},
    tasks::{new_tokio_cancellation_token, new_tokio_task_tracker},
    CkbConfig, NetworkServiceEvent,
};

use super::{
    channel::{ChannelActorState, ChannelActorStateStore, ChannelState},
    types::Hash256,
    NetworkActor, NetworkActorCommand, NetworkActorMessage,
};

static RETAIN_VAR: &str = "TEST_TEMP_RETAIN";

#[derive(Debug)]
pub struct TempDir(ManuallyDrop<OldTempDir>);

impl TempDir {
    fn new<S: AsRef<OsStr>>(prefix: S) -> Self {
        Self(ManuallyDrop::new(
            OldTempDir::with_prefix(prefix).expect("create temp directory"),
        ))
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let retain = env::var(RETAIN_VAR);
        if retain.is_ok() {
            println!(
                "Keeping temp directory {:?}, as environment variable {RETAIN_VAR} set",
                self.as_ref()
            );
        } else {
            println!(
                "Deleting temp directory {:?}. To keep this directory, set environment variable {RETAIN_VAR} to anything",
                self.as_ref()
            );
            unsafe {
                ManuallyDrop::drop(&mut self.0);
            }
        }
    }
}

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
    pub chain_actor: ActorRef<CkbChainMessage>,
    pub peer_id: PeerId,
    pub event_emitter: mpsc::Receiver<NetworkServiceEvent>,
}

impl NetworkNode {
    pub async fn new() -> Self {
        let base_dir = TempDir::new("cfn-node-test");
        let ckb_config = CkbConfig {
            base_dir: Some(PathBuf::from(base_dir.as_ref())),
            auto_accept_channel_ckb_funding_amount: Some(0), // Disable auto accept for unit tests
            ..Default::default()
        };

        let root = ROOT_ACTOR.get_or_init(get_test_root_actor).await.clone();
        let (event_sender, mut event_receiver) = mpsc::channel(10000);

        let mut chain_base_dir = PathBuf::from(base_dir.as_ref());
        chain_base_dir.push("ckb-chain");

        let chain_actor = Actor::spawn_linked(None, MockChainActor::new(), (), root.get_cell())
            .await
            .expect("start mock chain actor")
            .0;

        let network_actor = Actor::spawn_linked(
            Some(format!("network actor at {:?}", base_dir.as_ref())),
            NetworkActor::new(event_sender, chain_actor.clone(), MemoryStore::default()),
            (ckb_config, new_tokio_task_tracker()),
            root.get_cell(),
        )
        .await
        .expect("start network actor")
        .0;

        #[allow(clippy::never_loop)]
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

        println!(
            "Network node started for peer_id {:?} in directory {:?}",
            &peer_id,
            base_dir.as_ref()
        );

        Self {
            base_dir,
            listening_addr,
            network_actor,
            chain_actor,
            peer_id,
            event_emitter: event_receiver,
        }
    }

    pub async fn new_n_interconnected_nodes(n: usize) -> Vec<Self> {
        let mut nodes: Vec<NetworkNode> = Vec::with_capacity(n);
        for _ in 0..n {
            let new = Self::new().await;
            for node in nodes.iter_mut() {
                node.connect_to(&new).await;
            }
            nodes.push(new);
        }
        nodes
    }

    pub async fn connect_to(&mut self, other: &Self) {
        let peer_addr = other.listening_addr.clone();
        let peer_id = other.peer_id.clone();
        println!(
            "Trying to connect to {:?} from {:?}",
            other.listening_addr, &self.listening_addr
        );

        self.network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::ConnectPeer(peer_addr.clone()),
            ))
            .expect("self alive");

        self.expect_event(|event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == &peer_id))
        .await;
    }

    pub async fn expect_to_process_event<F, T>(&mut self, event_processor: F) -> T
    where
        F: Fn(&NetworkServiceEvent) -> Option<T>,
    {
        loop {
            select! {
                event = self.event_emitter.recv() => {
                    match event {
                        None => panic!("Event emitter unexpectedly stopped"),
                        Some(event) => {
                            println!("Recevied event when waiting for specific event: {:?}", &event);
                            if let Some(r) = event_processor(&event) {
                                println!("Event ({:?}) matching filter received, exiting waiting for event loop", &event);
                                return r;
                            }
                        }
                    }
                }
                _ = sleep(Duration::from_secs(5)) => {
                    panic!("Waiting for event timeout");
                }
            }
        }
    }

    pub async fn expect_event<F>(&mut self, event_filter: F)
    where
        F: Fn(&NetworkServiceEvent) -> bool,
    {
        self.expect_to_process_event(|event| if event_filter(event) { Some(()) } else { None })
            .await;
    }

    pub async fn submit_tx(&mut self, tx: TransactionView) -> ckb_jsonrpc_types::Status {
        submit_tx(self.chain_actor.clone(), tx).await
    }

    pub async fn trace_tx(&mut self, tx: TransactionView) -> ckb_jsonrpc_types::Status {
        trace_tx(self.chain_actor.clone(), tx).await
    }
}

#[derive(Clone, Default)]
struct MemoryStore {
    channel_actor_state_map: Arc<RwLock<HashMap<Hash256, ChannelActorState>>>,
}

impl ChannelActorStateStore for MemoryStore {
    fn get_channel_actor_state(&self, id: &Hash256) -> Option<ChannelActorState> {
        self.channel_actor_state_map
            .read()
            .unwrap()
            .get(id)
            .cloned()
    }

    fn insert_channel_actor_state(&self, state: ChannelActorState) {
        self.channel_actor_state_map
            .write()
            .unwrap()
            .insert(state.id, state);
    }

    fn delete_channel_actor_state(&self, id: &Hash256) {
        self.channel_actor_state_map.write().unwrap().remove(id);
    }

    fn get_channel_ids_by_peer(&self, peer_id: &PeerId) -> Vec<Hash256> {
        self.channel_actor_state_map
            .read()
            .unwrap()
            .values()
            .filter_map(|state| {
                if peer_id == &state.peer_id {
                    Some(state.id)
                } else {
                    None
                }
            })
            .collect()
    }

    fn get_channel_states(&self, peer_id: Option<PeerId>) -> Vec<(PeerId, Hash256, ChannelState)> {
        let map = self.channel_actor_state_map.read().unwrap();
        let values = map.values();
        match peer_id {
            Some(peer_id) => values
                .filter_map(|state| {
                    if peer_id == state.peer_id {
                        Some((state.peer_id.clone(), state.id, state.state))
                    } else {
                        None
                    }
                })
                .collect(),
            None => values
                .map(|state| (state.peer_id.clone(), state.id, state.state))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::NetworkNode;

    #[tokio::test]
    async fn test_start_network_node() {
        println!("starting network node");
        let node = NetworkNode::new().await;
        println!("network node {:?} started", &node);
    }

    #[tokio::test]
    async fn test_connect_to_other_node() {
        let mut node_a = NetworkNode::new().await;
        let node_b = NetworkNode::new().await;
        node_a.connect_to(&node_b).await;
    }

    #[tokio::test]
    async fn test_create_two_interconnected_nodes() {
        let _two_nodes = NetworkNode::new_n_interconnected_nodes(2).await;
    }
}
