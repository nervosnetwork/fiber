use crate::fiber::graph::{ChannelInfo, NetworkGraph, NodeInfo};
use crate::fiber::types::Pubkey;
use crate::invoice::{CkbInvoice, InvoiceError, InvoiceStore};
use ckb_jsonrpc_types::JsonBytes;
use ckb_types::packed::OutPoint;
use ckb_types::{core::TransactionView, packed::Byte32};
use ractor::{Actor, ActorRef};
use rand::Rng;
use secp256k1::{rand, PublicKey, Secp256k1, SecretKey};
use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    mem::ManuallyDrop,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tempfile::TempDir as OldTempDir;
use tentacle::multiaddr::Multiaddr;
use tentacle::{multiaddr::MultiAddr, secio::PeerId};
use tokio::sync::RwLock as TokioRwLock;
use tokio::{
    select,
    sync::{mpsc, OnceCell},
    time::sleep,
};

use crate::{
    actors::{RootActor, RootActorMessage},
    ckb::{submit_tx, trace_tx, trace_tx_hash, CkbChainMessage, MockChainActor},
    fiber::network::NetworkActorStartArguments,
    tasks::{new_tokio_cancellation_token, new_tokio_task_tracker},
    FiberConfig, NetworkServiceEvent,
};

use super::graph::PaymentSession;
use super::{
    channel::{ChannelActorState, ChannelActorStateStore, ChannelState},
    types::Hash256,
    NetworkActor, NetworkActorCommand, NetworkActorMessage,
};
use crate::fiber::graph::NetworkGraphStateStore;

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

pub fn generate_keypair() -> (SecretKey, PublicKey) {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::new(&mut rand::thread_rng());
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    (secret_key, public_key)
}

pub fn generate_seckey() -> SecretKey {
    SecretKey::new(&mut rand::thread_rng())
}

pub fn generate_pubkey() -> PublicKey {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::new(&mut rand::thread_rng());
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    public_key
}

pub fn gen_sha256_hash() -> Hash256 {
    let mut rng = rand::thread_rng();
    let mut result = [0u8; 32];
    rng.fill(&mut result[..]);
    result.into()
}

#[derive(Debug)]
pub struct NetworkNode {
    /// The base directory of the node, will be deleted after this struct dropped.
    pub base_dir: TempDir,
    pub listening_addrs: Vec<MultiAddr>,
    pub network_actor: ActorRef<NetworkActorMessage>,
    pub chain_actor: ActorRef<CkbChainMessage>,
    pub peer_id: PeerId,
    pub event_emitter: mpsc::Receiver<NetworkServiceEvent>,
}

impl NetworkNode {
    pub async fn new() -> Self {
        Self::new_with_node_name(None).await
    }

    pub async fn new_with_node_name(node_name: Option<String>) -> Self {
        let base_dir = TempDir::new("fnn-test");
        let fiber_config = FiberConfig {
            announced_node_name: node_name
                .as_deref()
                .or(base_dir.as_ref().file_name().unwrap().to_str())
                .map(Into::into),
            announce_listening_addr: Some(true),
            base_dir: Some(PathBuf::from(base_dir.as_ref())),
            auto_accept_channel_ckb_funding_amount: Some(0), // Disable auto accept for unit tests
            ..Default::default()
        };

        let root = ROOT_ACTOR.get_or_init(get_test_root_actor).await.clone();
        let (event_sender, mut event_receiver) = mpsc::channel(10000);

        let chain_actor = Actor::spawn_linked(None, MockChainActor::new(), (), root.get_cell())
            .await
            .expect("start mock chain actor")
            .0;

        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let public_key = PublicKey::from_secret_key(&secp, &secret_key);
        let network_graph = Arc::new(TokioRwLock::new(NetworkGraph::new(
            MemoryStore::default(),
            public_key.into(),
        )));
        let network_actor = Actor::spawn_linked(
            Some(format!("network actor at {:?}", base_dir.as_ref())),
            NetworkActor::new(
                event_sender,
                chain_actor.clone(),
                MemoryStore::default(),
                network_graph,
            ),
            NetworkActorStartArguments {
                config: fiber_config,
                tracker: new_tokio_task_tracker(),
                channel_subscribers: Default::default(),
            },
            root.get_cell(),
        )
        .await
        .expect("start network actor")
        .0;

        #[allow(clippy::never_loop)]
        let (peer_id, _listening_addr, announced_addrs) = loop {
            select! {
                Some(NetworkServiceEvent::NetworkStarted(peer_id, listening_addr, announced_addrs)) = event_receiver.recv() => {
                    break (peer_id, listening_addr, announced_addrs);
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
            listening_addrs: announced_addrs,
            network_actor,
            chain_actor,
            peer_id,
            event_emitter: event_receiver,
        }
    }

    pub async fn new_n_interconnected_nodes(n: usize) -> Vec<Self> {
        let mut nodes: Vec<NetworkNode> = Vec::with_capacity(n);
        for i in 0..n {
            let new = Self::new_with_node_name(Some(format!("Node {i}"))).await;
            for node in nodes.iter_mut() {
                node.connect_to(&new).await;
            }
            nodes.push(new);
        }
        nodes
    }

    pub async fn connect_to_nonblocking(&mut self, other: &Self) {
        let peer_addr = other.listening_addrs[0].clone();
        println!(
            "Trying to connect to {:?} from {:?}",
            other.listening_addrs, &self.listening_addrs
        );

        self.network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::ConnectPeer(peer_addr.clone()),
            ))
            .expect("self alive");
    }

    pub async fn connect_to(&mut self, other: &Self) {
        self.connect_to_nonblocking(other).await;
        let peer_id = &other.peer_id;
        self.expect_event(
            |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == peer_id),
        )
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

    pub async fn trace_tx_hash(&mut self, tx_hash: Byte32) -> ckb_jsonrpc_types::Status {
        trace_tx_hash(self.chain_actor.clone(), tx_hash).await
    }
}

#[derive(Clone, Default)]
struct MemoryStore {
    channel_actor_state_map: Arc<RwLock<HashMap<Hash256, ChannelActorState>>>,
    channels_map: Arc<RwLock<HashMap<OutPoint, ChannelInfo>>>,
    nodes_map: Arc<RwLock<HashMap<Pubkey, NodeInfo>>>,
    connected_peer_addresses: Arc<RwLock<HashMap<PeerId, Multiaddr>>>,
    payment_sessions: Arc<RwLock<HashMap<Hash256, PaymentSession>>>,
    invoice_store: Arc<RwLock<HashMap<Hash256, CkbInvoice>>>,
    invoice_hash_to_preimage: Arc<RwLock<HashMap<Hash256, Hash256>>>,
}

impl NetworkGraphStateStore for MemoryStore {
    fn get_channels(&self, outpoint: Option<OutPoint>) -> Vec<ChannelInfo> {
        if let Some(outpoint) = outpoint {
            let mut res = vec![];

            if let Some(channel) = self.channels_map.read().unwrap().get(&outpoint) {
                res.push(channel.clone());
            }
            res
        } else {
            self.channels_map
                .read()
                .unwrap()
                .values()
                .cloned()
                .collect()
        }
    }

    fn insert_channel(&self, channel: ChannelInfo) {
        self.channels_map
            .write()
            .unwrap()
            .insert(channel.out_point(), channel);
    }

    fn get_nodes(&self, node_id: Option<Pubkey>) -> Vec<NodeInfo> {
        if let Some(node_id) = node_id {
            let mut res = vec![];

            if let Some(node) = self.nodes_map.read().unwrap().get(&node_id) {
                res.push(node.clone());
            }
            res
        } else {
            self.nodes_map.read().unwrap().values().cloned().collect()
        }
    }

    fn get_nodes_with_params(
        &self,
        limit: usize,
        after: Option<JsonBytes>,
        node_id: Option<Pubkey>,
    ) -> (Vec<NodeInfo>, JsonBytes) {
        unimplemented!("currently not used in mock store");
    }

    fn get_channels_with_params(
        &self,
        limit: usize,
        after: Option<JsonBytes>,
        outpoint: Option<OutPoint>,
    ) -> (Vec<ChannelInfo>, JsonBytes) {
        unimplemented!("currently not used in mock store");
    }

    fn insert_node(&self, node: NodeInfo) {
        self.nodes_map
            .write()
            .unwrap()
            .insert(node.node_id.clone(), node);
    }

    fn insert_connected_peer(&self, peer_id: PeerId, multiaddr: Multiaddr) {
        self.connected_peer_addresses
            .write()
            .unwrap()
            .insert(peer_id, multiaddr);
    }

    fn get_connected_peer(&self, peer_id: Option<PeerId>) -> Vec<(PeerId, Multiaddr)> {
        if let Some(peer_id) = peer_id {
            let mut res = vec![];

            if let Some(addr) = self.connected_peer_addresses.read().unwrap().get(&peer_id) {
                res.push((peer_id, addr.clone()));
            }
            res
        } else {
            self.connected_peer_addresses
                .read()
                .unwrap()
                .iter()
                .map(|(peer_id, addr)| (peer_id.clone(), addr.clone()))
                .collect()
        }
    }

    fn remove_connected_peer(&self, peer_id: &PeerId) {
        self.connected_peer_addresses
            .write()
            .unwrap()
            .remove(peer_id);
    }

    fn get_payment_session(&self, id: Hash256) -> Option<PaymentSession> {
        self.payment_sessions.read().unwrap().get(&id).cloned()
    }

    fn insert_payment_session(&self, session: PaymentSession) {
        self.payment_sessions
            .write()
            .unwrap()
            .insert(session.payment_hash(), session);
    }
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
                if peer_id == &state.get_remote_peer_id() {
                    Some(state.id.clone())
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
                    if peer_id == state.get_remote_peer_id() {
                        Some((state.get_remote_peer_id(), state.id, state.state.clone()))
                    } else {
                        None
                    }
                })
                .collect(),
            None => values
                .map(|state| {
                    (
                        state.get_remote_peer_id(),
                        state.id.clone(),
                        state.state.clone(),
                    )
                })
                .collect(),
        }
    }
}

impl InvoiceStore for MemoryStore {
    fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice> {
        self.invoice_store.read().unwrap().get(id).cloned()
    }

    fn insert_invoice(
        &self,
        invoice: CkbInvoice,
        preimage: Option<Hash256>,
    ) -> Result<(), InvoiceError> {
        let id = invoice.payment_hash();
        if let Some(preimage) = preimage {
            self.invoice_hash_to_preimage
                .write()
                .unwrap()
                .insert(*id, preimage);
        }
        self.invoice_store.write().unwrap().insert(*id, invoice);
        Ok(())
    }

    fn get_invoice_preimage(&self, hash: &Hash256) -> Option<Hash256> {
        self.invoice_hash_to_preimage
            .read()
            .unwrap()
            .get(hash)
            .cloned()
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
