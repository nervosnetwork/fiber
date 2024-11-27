use crate::fiber::types::EcdsaSignature;
use crate::fiber::types::Pubkey;
use ckb_types::{core::TransactionView, packed::Byte32};
use ractor::{Actor, ActorRef};
use rand::rngs::OsRng;
use rand::Rng;
use secp256k1::Keypair;
use secp256k1::{rand, Message, PublicKey, Secp256k1, SecretKey};
use std::{
    env,
    ffi::OsStr,
    mem::ManuallyDrop,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir as OldTempDir;
use tentacle::{multiaddr::MultiAddr, secio::PeerId};
use tokio::sync::RwLock as TokioRwLock;
use tokio::{
    select,
    sync::{mpsc, OnceCell},
    time::sleep,
};

use crate::store::Store;
use crate::{
    actors::{RootActor, RootActorMessage},
    ckb::tests::test_utils::{
        get_tx_from_hash, submit_tx, trace_tx, trace_tx_hash, MockChainActor,
    },
    ckb::CkbChainMessage,
    fiber::graph::NetworkGraph,
    fiber::network::{
        NetworkActor, NetworkActorCommand, NetworkActorMessage, NetworkActorStartArguments,
    },
    fiber::types::Hash256,
    tasks::{new_tokio_cancellation_token, new_tokio_task_tracker},
    FiberConfig, NetworkServiceEvent,
};
use tentacle::secio::SecioKeyPair;

static RETAIN_VAR: &str = "TEST_TEMP_RETAIN";

#[derive(Debug)]
pub struct TempDir(ManuallyDrop<OldTempDir>);

impl TempDir {
    fn new<S: AsRef<OsStr>>(prefix: S) -> Self {
        Self(ManuallyDrop::new(
            OldTempDir::with_prefix(prefix).expect("create temp directory"),
        ))
    }

    pub fn to_str(&self) -> &str {
        self.0.path().to_str().expect("path to str")
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

pub fn init_tracing() {
    use std::sync::Once;

    static INIT: Once = Once::new();

    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .pretty()
            .init();
    });
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

pub fn generate_pubkey() -> Pubkey {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::new(&mut rand::thread_rng());
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    public_key.into()
}

pub fn gen_sha256_hash() -> Hash256 {
    let mut rng = rand::thread_rng();
    let mut result = [0u8; 32];
    rng.fill(&mut result[..]);
    result.into()
}

pub fn get_fiber_config<P: AsRef<Path>>(base_dir: P, node_name: Option<&str>) -> FiberConfig {
    let base_dir = base_dir.as_ref();
    FiberConfig {
        announced_node_name: node_name
            .or(base_dir.file_name().unwrap().to_str())
            .map(Into::into),
        announce_listening_addr: Some(true),
        base_dir: Some(PathBuf::from(base_dir)),
        auto_accept_channel_ckb_funding_amount: Some(0), // Disable auto accept for unit tests
        announce_private_addr: Some(true),               // Announce private address for unit tests
        ..Default::default()
    }
}

// Mock function to create a dummy EcdsaSignature
pub fn mock_ecdsa_signature() -> EcdsaSignature {
    let secp = Secp256k1::new();
    let mut rng = OsRng::default();
    let (secret_key, _public_key) = secp.generate_keypair(&mut rng);
    let message = Message::from_digest_slice(&[0u8; 32]).expect("32 bytes");
    let signature = secp.sign_ecdsa(&message, &secret_key);
    EcdsaSignature(signature)
}

pub fn generate_store() -> Store {
    let temp_dir = TempDir::new("fnn-test");
    let store = Store::new(temp_dir.as_ref());
    store.expect("create store")
}

pub struct NetworkNode {
    /// The base directory of the node, will be deleted after this struct dropped.
    pub base_dir: Arc<TempDir>,
    pub node_name: Option<String>,
    pub store: Store,
    pub fiber_config: FiberConfig,
    pub listening_addrs: Vec<MultiAddr>,
    pub network_actor: ActorRef<NetworkActorMessage>,
    pub chain_actor: ActorRef<CkbChainMessage>,
    pub peer_id: PeerId,
    pub event_emitter: mpsc::Receiver<NetworkServiceEvent>,
    pub pubkey: Pubkey,
}

impl NetworkNode {
    pub fn get_node_address(&self) -> &MultiAddr {
        &self.listening_addrs[0]
    }
}

pub struct NetworkNodeConfig {
    base_dir: Arc<TempDir>,
    node_name: Option<String>,
    store: Store,
    fiber_config: FiberConfig,
}

impl NetworkNodeConfig {
    pub fn builder() -> NetworkNodeConfigBuilder {
        NetworkNodeConfigBuilder::new()
    }
}

pub struct NetworkNodeConfigBuilder {
    base_dir: Option<Arc<TempDir>>,
    node_name: Option<String>,
    // We may generate a FiberConfig based on the base_dir and node_name,
    // but allow user to override it.
    fiber_config_updater: Option<Box<dyn FnOnce(&mut FiberConfig) + 'static>>,
}

impl NetworkNodeConfigBuilder {
    pub fn new() -> Self {
        Self {
            base_dir: None,
            node_name: None,
            fiber_config_updater: None,
        }
    }

    pub fn base_dir(mut self, base_dir: Arc<TempDir>) -> Self {
        self.base_dir = Some(base_dir);
        self
    }

    pub fn base_dir_prefix(self, prefix: &str) -> Self {
        self.base_dir(Arc::new(TempDir::new(prefix)))
    }

    pub fn node_name(mut self, node_name: Option<String>) -> Self {
        self.node_name = node_name;
        self
    }

    pub fn fiber_config_updater(
        mut self,
        updater: impl FnOnce(&mut FiberConfig) + 'static,
    ) -> Self {
        self.fiber_config_updater = Some(Box::new(updater));
        self
    }

    pub fn build(self) -> NetworkNodeConfig {
        let base_dir = self
            .base_dir
            .clone()
            .unwrap_or_else(|| Arc::new(TempDir::new("fnn-test")));
        let node_name = self.node_name.clone();
        let store = generate_store();
        let fiber_config = get_fiber_config(base_dir.as_ref(), node_name.as_deref());
        let mut config = NetworkNodeConfig {
            base_dir,
            node_name,
            store,
            fiber_config,
        };
        if let Some(updater) = self.fiber_config_updater {
            updater(&mut config.fiber_config);
        }
        config
    }
}

impl NetworkNode {
    pub async fn new() -> Self {
        Self::new_with_node_name_opt(None).await
    }

    pub async fn new_with_node_name(node_name: &str) -> Self {
        let config = NetworkNodeConfigBuilder::new()
            .node_name(Some(node_name.to_string()))
            .build();
        Self::new_with_config(config).await
    }

    pub async fn new_with_node_name_opt(node_name: Option<String>) -> Self {
        let config = NetworkNodeConfigBuilder::new().node_name(node_name).build();
        Self::new_with_config(config).await
    }

    pub async fn new_with_config(config: NetworkNodeConfig) -> Self {
        let NetworkNodeConfig {
            base_dir,
            node_name,
            store,
            fiber_config,
        } = config;

        let _span = tracing::info_span!("NetworkNode", node_name = &node_name).entered();

        let root = ROOT_ACTOR.get_or_init(get_test_root_actor).await.clone();
        let (event_sender, mut event_receiver) = mpsc::channel(10000);

        let chain_actor = Actor::spawn_linked(None, MockChainActor::new(), (), root.get_cell())
            .await
            .expect("start mock chain actor")
            .0;

        let kp = fiber_config
            .read_or_generate_secret_key()
            .expect("read or generate secret key");
        let secio_kp = SecioKeyPair::from(kp);
        let public_key: Pubkey = secio_kp.public_key().into();

        let network_graph = Arc::new(TokioRwLock::new(NetworkGraph::new(
            store.clone(),
            public_key.clone(),
        )));
        let network_actor = Actor::spawn_linked(
            Some(format!("network actor at {}", base_dir.to_str())),
            NetworkActor::new(
                event_sender,
                chain_actor.clone(),
                store.clone(),
                network_graph,
            ),
            NetworkActorStartArguments {
                config: fiber_config.clone(),
                tracker: new_tokio_task_tracker(),
                channel_subscribers: Default::default(),
                default_shutdown_script: Default::default(),
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
            node_name,
            store,
            fiber_config,
            listening_addrs: announced_addrs,
            network_actor,
            chain_actor,
            peer_id,
            event_emitter: event_receiver,
            pubkey: public_key.into(),
        }
    }

    pub fn get_node_config(&self) -> NetworkNodeConfig {
        NetworkNodeConfig {
            base_dir: self.base_dir.clone(),
            node_name: self.node_name.clone(),
            store: self.store.clone(),
            fiber_config: self.fiber_config.clone(),
        }
    }

    pub async fn start(&mut self) {
        let config = self.get_node_config();
        let new = Self::new_with_config(config).await;
        *self = new;
    }

    pub async fn stop(&mut self) {
        self.network_actor
            .stop(Some("stopping actor on request".to_string()));
        let my_peer_id = self.peer_id.clone();
        self.expect_event(
            |event| matches!(event, NetworkServiceEvent::NetworkStopped(id) if id == &my_peer_id),
        )
        .await;
    }

    pub async fn restart(&mut self) {
        self.stop().await;
        // Tentacle shutdown may require some time to propagate to other nodes.
        // If we start the node immediately, other nodes may deem our new connection
        // as a duplicate connection and report RepeatedConnection error.
        // And we will receive `ProtocolSelectError` error from tentacle.
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        tracing::debug!("Node stopped, restarting");
        self.start().await;
    }

    pub async fn new_n_interconnected_nodes<const N: usize>() -> [Self; N] {
        let mut nodes: Vec<NetworkNode> = Vec::with_capacity(N);
        for i in 0..N {
            let new = Self::new_with_config(
                NetworkNodeConfigBuilder::new()
                    .node_name(Some(format!("node-{}", i)))
                    .base_dir_prefix(&format!("fnn-test-node-{}-", i))
                    .build(),
            )
            .await;
            for node in nodes.iter_mut() {
                node.connect_to(&new).await;
            }
            nodes.push(new);
        }
        match nodes.try_into() {
            Ok(nodes) => nodes,
            Err(_) => unreachable!(),
        }
    }

    // Create n nodes and connect them. The config_gen function
    // (function that creates a NetworkNodeConfig from an index)
    // will be called to generate the config for each node.
    pub async fn new_n_interconnected_nodes_with_config(
        n: usize,
        config_gen: impl Fn(usize) -> NetworkNodeConfig,
    ) -> Vec<Self> {
        let mut nodes: Vec<NetworkNode> = Vec::with_capacity(n);
        for i in 0..n {
            let new = Self::new_with_config(config_gen(i)).await;
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

    pub async fn get_tx_from_hash(
        &mut self,
        tx_hash: Byte32,
    ) -> Result<TransactionView, anyhow::Error> {
        get_tx_from_hash(self.chain_actor.clone(), tx_hash).await
    }
}

pub(crate) fn rand_sha256_hash() -> Hash256 {
    let mut rng = rand::thread_rng();
    let mut result = [0u8; 32];
    rng.fill(&mut result[..]);
    result.into()
}

pub(crate) fn gen_rand_public_key() -> Pubkey {
    let secp = Secp256k1::new();
    let key_pair = Keypair::new(&secp, &mut rand::thread_rng());
    PublicKey::from_keypair(&key_pair).into()
}

pub(crate) fn gen_rand_private_key() -> SecretKey {
    let secp = Secp256k1::new();
    let key_pair = Keypair::new(&secp, &mut rand::thread_rng());
    SecretKey::from_keypair(&key_pair)
}

pub(crate) fn gen_rand_keypair() -> (PublicKey, SecretKey) {
    let secp = Secp256k1::new();
    let key_pair = Keypair::new(&secp, &mut rand::thread_rng());
    (
        PublicKey::from_keypair(&key_pair),
        SecretKey::from_keypair(&key_pair),
    )
}

#[tokio::test]
async fn test_connect_to_other_node() {
    let mut node_a = NetworkNode::new().await;
    let node_b = NetworkNode::new().await;
    node_a.connect_to(&node_b).await;
}

#[tokio::test]
async fn test_restart_network_node() {
    let mut node = NetworkNode::new().await;
    node.restart().await;
}
