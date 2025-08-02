use crate::ckb::tests::test_utils::get_tx_from_hash;
use crate::ckb::tests::test_utils::MockChainActorMiddleware;
use crate::ckb::CkbConfig;
use crate::ckb::GetTxResponse;
use crate::fiber::channel::*;
use crate::fiber::config::CKB_SHANNONS;
use crate::fiber::features::FeatureVector;
use crate::fiber::gossip::get_gossip_actor_name;
use crate::fiber::gossip::GossipActorMessage;
use crate::fiber::graph::NetworkGraphStateStore;
use crate::fiber::graph::PaymentSession;
use crate::fiber::graph::PaymentStatus;
use crate::fiber::graph::SessionRoute;
use crate::fiber::network::*;
use crate::fiber::types::EcdsaSignature;
use crate::fiber::types::FiberMessage;
use crate::fiber::types::GossipMessage;
use crate::fiber::types::Init;
use crate::fiber::types::Pubkey;
use crate::fiber::types::Shutdown;
use crate::fiber::ASSUME_NETWORK_ACTOR_ALIVE;
use crate::gen_rand_sha256_hash;
use crate::invoice::*;
use crate::rpc::config::RpcConfig;
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::invoice::InvoiceResult;
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::invoice::NewInvoiceParams;
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::server::start_rpc;
use ckb_sdk::core::TransactionBuilder;
use ckb_types::core::FeeRate;
use ckb_types::{
    core::{tx_pool::TxStatus, TransactionView},
    packed::{OutPoint, Script},
};
use hyper::header::HeaderValue;
use hyper::HeaderMap;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::core::client::ClientT;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::http_client::transport::HttpBackend;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::http_client::HttpClient;
use jsonrpsee::rpc_params;
#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::server::ServerHandle;
use ractor::{call, Actor, ActorRef};
use rand::distributions::Alphanumeric;
use rand::rngs::OsRng;
use rand::Rng;
use secp256k1::{Message, Secp256k1};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Instant;
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
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::fiber::graph::ChannelInfo;
use crate::fiber::graph::NodeInfo;
use crate::fiber::network::{AcceptChannelCommand, OpenChannelCommand};
use crate::fiber::types::Privkey;
use crate::store::Store;
use crate::{
    actors::{RootActor, RootActorMessage},
    ckb::tests::test_utils::{submit_tx, trace_tx, MockChainActor},
    ckb::CkbChainMessage,
    fiber::graph::NetworkGraph,
    fiber::network::{
        NetworkActor, NetworkActorCommand, NetworkActorMessage, NetworkActorStartArguments,
    },
    fiber::types::Hash256,
    tasks::{new_tokio_cancellation_token, new_tokio_task_tracker},
    FiberConfig, NetworkServiceEvent,
};

static RETAIN_VAR: &str = "TEST_TEMP_RETAIN";
pub const MIN_RESERVED_CKB: u128 = 42 * CKB_SHANNONS as u128;
pub const HUGE_CKB_AMOUNT: u128 = MIN_RESERVED_CKB + 1000000 * CKB_SHANNONS as u128;
const DEFAULT_WAIT_UNTIL_TIME: u64 = 60 * 5; // seconds

#[derive(Debug)]
pub struct TempDir(ManuallyDrop<OldTempDir>);

impl TempDir {
    pub fn new<S: AsRef<OsStr>>(prefix: S) -> Self {
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
            warn!(
                "Keeping temp directory {:?}, as environment variable {RETAIN_VAR} set",
                self.as_ref()
            );
        } else {
            warn!(
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

pub fn gen_rpc_config() -> RpcConfig {
    RpcConfig {
        listening_addr: None,
        biscuit_public_key: None,
        enabled_modules: vec![
            "info".to_string(),
            "channel".to_string(),
            "graph".to_string(),
            "payment".to_string(),
            "invoice".to_string(),
            "peer".to_string(),
            "watchtower".to_string(),
        ],
    }
}

static ROOT_ACTOR: OnceCell<ActorRef<RootActorMessage>> = OnceCell::const_new();

pub async fn get_test_root_actor() -> ActorRef<RootActorMessage> {
    use futures::FutureExt;
    // Only one actor with the same name can be created.
    ROOT_ACTOR
        .get_or_init(|| {
            Actor::spawn(
                Some("test root actor".to_string()),
                RootActor {},
                (new_tokio_task_tracker(), new_tokio_cancellation_token()),
            )
            .map(|r| r.expect("start test root actor").0)
        })
        .await
        .clone()
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
    let mut rng = OsRng;
    let (secret_key, _public_key) = secp.generate_keypair(&mut rng);
    let message = Message::from_digest_slice(&[0u8; 32]).expect("32 bytes");
    let signature = secp.sign_ecdsa(&message, &secret_key);
    EcdsaSignature(signature)
}

pub fn generate_store() -> (Store, TempDir) {
    let temp_dir = TempDir::new("test-fnn-node");
    let store = Store::new(temp_dir.as_ref());
    (store.expect("create store"), temp_dir)
}

#[derive(Debug)]
pub struct NetworkNode {
    /// The base directory of the node, will be deleted after this struct dropped.
    pub base_dir: Arc<TempDir>,
    pub node_name: Option<String>,
    pub store: Store,
    pub channels_tx_map: HashMap<Hash256, Hash256>,
    pub fiber_config: FiberConfig,
    pub rpc_config: Option<RpcConfig>,
    pub ckb_config: Option<CkbConfig>,
    pub listening_addrs: Vec<MultiAddr>,
    pub network_actor: ActorRef<NetworkActorMessage>,
    pub ckb_chain_actor: ActorRef<CkbChainMessage>,
    pub network_graph: Arc<TokioRwLock<NetworkGraph<Store>>>,
    pub chain_actor: ActorRef<CkbChainMessage>,
    pub mock_chain_actor_middleware: Option<Box<dyn MockChainActorMiddleware>>,
    pub gossip_actor: ActorRef<GossipActorMessage>,
    pub private_key: Privkey,
    pub peer_id: PeerId,
    pub event_emitter: mpsc::Receiver<NetworkServiceEvent>,
    pub pubkey: Pubkey,
    pub unexpected_events: Arc<TokioRwLock<HashSet<String>>>,
    pub triggered_unexpected_events: Arc<TokioRwLock<Vec<String>>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub rpc_server: Option<(ServerHandle, SocketAddr)>,
    pub auth_token: Option<String>,
}

pub struct NetworkNodeConfig {
    base_dir: Arc<TempDir>,
    node_name: Option<String>,
    store: Store,
    fiber_config: FiberConfig,
    rpc_config: Option<RpcConfig>,
    ckb_config: Option<CkbConfig>,
    mock_chain_actor_middleware: Option<Box<dyn MockChainActorMiddleware>>,
}

impl NetworkNodeConfig {
    pub fn builder() -> NetworkNodeConfigBuilder {
        NetworkNodeConfigBuilder::new()
    }
}

pub struct NetworkNodeConfigBuilder {
    base_dir: Option<Arc<TempDir>>,
    node_name: Option<String>,
    rpc_config: Option<RpcConfig>,
    // We may generate a FiberConfig based on the base_dir and node_name,
    // but allow user to override it.
    #[allow(clippy::type_complexity)]
    fiber_config_updater: Option<Box<dyn FnOnce(&mut FiberConfig) + 'static>>,
    mock_chain_actor_middleware: Option<Box<dyn MockChainActorMiddleware>>,
}

impl Default for NetworkNodeConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkNodeConfigBuilder {
    pub fn new() -> Self {
        Self {
            base_dir: None,
            node_name: None,
            rpc_config: None,
            fiber_config_updater: None,
            mock_chain_actor_middleware: None,
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

    pub fn rpc_config(mut self, rpc_config: Option<RpcConfig>) -> Self {
        self.rpc_config = rpc_config;
        self
    }

    pub fn fiber_config_updater(
        mut self,
        updater: impl FnOnce(&mut FiberConfig) + 'static,
    ) -> Self {
        self.fiber_config_updater = Some(Box::new(updater));
        self
    }

    pub fn mock_chain_actor_middleware(
        mut self,
        middleware: Box<dyn MockChainActorMiddleware>,
    ) -> Self {
        self.mock_chain_actor_middleware = Some(middleware);
        self
    }

    pub fn build(self) -> NetworkNodeConfig {
        let base_dir = self
            .base_dir
            .clone()
            .unwrap_or_else(|| Arc::new(TempDir::new("test-fnn-node")));
        let node_name = self.node_name.clone();

        // generate a random string as db name to avoid conflict
        // when build multiple nodes in the same NetworkNodeConfig
        let rand_name: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(5)
            .map(char::from)
            .collect();
        let rand_db_dir = Path::new(base_dir.to_str()).join(rand_name);
        let store = Store::new(rand_db_dir).expect("create store");
        let fiber_config = get_fiber_config(base_dir.as_ref(), node_name.as_deref());
        let ckb_config = if self.rpc_config.is_some() {
            let ckb_dir = Path::new(base_dir.to_str()).join("ckb");
            Some(CkbConfig {
                base_dir: Some(ckb_dir),
                rpc_url: "http://localhost:8114".to_string(),
                tx_tracing_polling_interval_ms: 4000,
                udt_whitelist: None,
                #[cfg(target_arch = "wasm32")]
                wasm_secret_key: None,
            })
        } else {
            None
        };
        let rpc_config = self.rpc_config;
        let mut config = NetworkNodeConfig {
            base_dir,
            ckb_config,
            node_name,
            store,
            fiber_config,
            rpc_config,
            mock_chain_actor_middleware: self.mock_chain_actor_middleware,
        };

        if let Some(updater) = self.fiber_config_updater {
            updater(&mut config.fiber_config);
        }
        config
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ChannelParameters {
    pub public: bool,
    pub node_a_funding_amount: u128,
    pub node_b_funding_amount: u128,
    pub a_max_tlc_number_in_flight: Option<u64>,
    pub a_max_tlc_value_in_flight: Option<u128>,
    pub a_tlc_expiry_delta: Option<u64>,
    pub a_tlc_min_value: Option<u128>,
    pub a_tlc_fee_proportional_millionths: Option<u128>,
    pub b_max_tlc_number_in_flight: Option<u64>,
    pub b_max_tlc_value_in_flight: Option<u128>,
    pub b_tlc_expiry_delta: Option<u64>,
    pub b_tlc_min_value: Option<u128>,
    pub b_tlc_fee_proportional_millionths: Option<u128>,
    pub funding_udt_type_script: Option<Script>,
}

impl ChannelParameters {
    pub fn new(node_a_funding_amount: u128, node_b_funding_amount: u128) -> Self {
        Self {
            public: true,
            node_a_funding_amount,
            node_b_funding_amount,
            ..Default::default()
        }
    }
}

pub(crate) async fn create_channel_with_nodes(
    node_a: &mut NetworkNode,
    node_b: &mut NetworkNode,
    params: ChannelParameters,
) -> Result<(Hash256, Hash256), String> {
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public: params.public,
                shutdown_script: None,
                funding_amount: params.node_a_funding_amount,
                funding_udt_type_script: params.funding_udt_type_script,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: params.a_tlc_expiry_delta,
                tlc_min_value: params.a_tlc_min_value,
                tlc_fee_proportional_millionths: params.a_tlc_fee_proportional_millionths,
                max_tlc_number_in_flight: params.a_max_tlc_number_in_flight,
                max_tlc_value_in_flight: params.a_max_tlc_value_in_flight,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .map_err(|e| e.to_string())?;

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                debug!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
                assert_eq!(peer_id, &node_a.peer_id);
                true
            }
            _ => false,
        })
        .await;

    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::AcceptChannel(
            AcceptChannelCommand {
                temp_channel_id: open_channel_result.channel_id,
                funding_amount: params.node_b_funding_amount,
                shutdown_script: None,
                max_tlc_number_in_flight: params.b_max_tlc_number_in_flight,
                max_tlc_value_in_flight: params.b_max_tlc_value_in_flight,
                min_tlc_value: params.b_tlc_min_value,
                tlc_fee_proportional_millionths: params.b_tlc_fee_proportional_millionths,
                tlc_expiry_delta: params.b_tlc_expiry_delta,
            },
            rpc_reply,
        ))
    };
    let accept_channel_result = call!(node_b.network_actor, message)
        .expect("node_b alive")
        .map_err(|e| e.to_string())?;

    let new_channel_id = accept_channel_result.new_channel_id;

    let funding_tx_outpoint = node_a
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, funding_tx_outpoint) => {
                debug!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_b.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                Some(funding_tx_outpoint.clone())
            }
            _ => None,
        })
        .await;

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, _funding_tx_hash) => {
                debug!(
                    "A channel ({:?}) to {:?} is now ready",
                    &channel_id, &peer_id
                );
                assert_eq!(peer_id, &node_a.peer_id);
                assert_eq!(channel_id, &new_channel_id);
                true
            }
            _ => false,
        })
        .await;

    let funding_tx_hash = funding_tx_outpoint.tx_hash().into();
    node_a.add_channel_tx(new_channel_id, funding_tx_hash);
    node_b.add_channel_tx(new_channel_id, funding_tx_hash);

    wait_for_network_graph_update(node_a, 1).await;

    Ok((new_channel_id, funding_tx_hash))
}

pub(crate) async fn establish_channel_between_nodes(
    node_a: &mut NetworkNode,
    node_b: &mut NetworkNode,
    params: ChannelParameters,
) -> (Hash256, Hash256) {
    create_channel_with_nodes(node_a, node_b, params)
        .await
        .expect("create channel between nodes")
}

#[cfg(test)]
pub(crate) async fn create_nodes_with_established_channel(
    node_a_funding_amount: u128,
    node_b_funding_amount: u128,
    public: bool,
) -> (NetworkNode, NetworkNode, Hash256) {
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let (channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters {
            public,
            node_a_funding_amount,
            node_b_funding_amount,
            ..Default::default()
        },
    )
    .await;

    (node_a, node_b, channel_id)
}

#[cfg(test)]
pub(crate) async fn create_3_nodes_with_established_channel(
    (channel_1_amount_a, channel_1_amount_b): (u128, u128),
    (channel_2_amount_b, channel_2_amount_c): (u128, u128),
) -> (NetworkNode, NetworkNode, NetworkNode, Hash256, Hash256) {
    let (nodes, channels) = create_n_nodes_with_established_channel(
        &[
            (channel_1_amount_a, channel_1_amount_b),
            (channel_2_amount_b, channel_2_amount_c),
        ],
        3,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");
    (node_a, node_b, node_c, channels[0], channels[1])
}

#[cfg(test)]
// make a network like A -> B -> C -> D
pub(crate) async fn create_n_nodes_with_established_channel(
    amounts: &[(u128, u128)],
    n: usize,
) -> (Vec<NetworkNode>, Vec<Hash256>) {
    assert!(n >= 2);
    assert_eq!(amounts.len(), n - 1);

    let nodes_index_map: Vec<((usize, usize), (u128, u128))> = (0..n - 1)
        .map(|i| ((i, i + 1), (amounts[i].0, amounts[i].1)))
        .collect();

    create_n_nodes_network(&nodes_index_map, n).await
}

#[allow(clippy::type_complexity)]
pub(crate) async fn create_n_nodes_network_with_params(
    amounts: &[((usize, usize), ChannelParameters)],
    n: usize,
    rpc_config: Option<RpcConfig>,
) -> (Vec<NetworkNode>, Vec<Hash256>) {
    assert!(n >= 2);
    let mut nodes = NetworkNode::new_interconnected_nodes(n, rpc_config).await;
    let mut channels = vec![];

    for ((i, j), channel_params) in amounts.iter() {
        let (channel_id, funding_tx) = {
            let (node_a, node_b) = {
                // avoid borrow nodes as mutable more than once
                assert_ne!(i, j);
                if i < j {
                    let (left, right) = nodes.split_at_mut(i + 1);
                    (&mut left[*i], &mut right[j - i - 1])
                } else {
                    let (left, right) = nodes.split_at_mut(j + 1);
                    (&mut right[i - j - 1], &mut left[*j])
                }
            };
            let (channel_id, funding_tx_hash) =
                establish_channel_between_nodes(node_a, node_b, channel_params.clone()).await;
            let funding_tx = node_a
                .get_transaction_view_from_hash(funding_tx_hash)
                .await
                .expect("get funding tx");

            (channel_id, funding_tx)
        };
        channels.push(channel_id);
        // all the other nodes submit_tx
        for node in nodes.iter_mut() {
            let res = node.submit_tx(funding_tx.clone()).await;
            node.add_channel_tx(channel_id, funding_tx.hash().into());
            assert!(matches!(res, TxStatus::Committed(..)));
        }
    }
    wait_for_network_graph_update(&nodes[0], amounts.len()).await;
    (nodes, channels)
}

#[allow(clippy::type_complexity)]
pub async fn create_n_nodes_network(
    amounts: &[((usize, usize), (u128, u128))],
    n: usize,
) -> (Vec<NetworkNode>, Vec<Hash256>) {
    let amounts = amounts
        .iter()
        .map(|((i, j), (a, b))| ((*i, *j), ChannelParameters::new(*a, *b)))
        .collect::<Vec<_>>();
    create_n_nodes_network_with_params(&amounts, n, None).await
}

impl NetworkNode {
    pub async fn new() -> Self {
        Self::new_with_node_name_opt(None).await
    }

    pub fn get_private_key(&self) -> &Privkey {
        &self.private_key
    }

    pub fn get_public_key(&self) -> Pubkey {
        self.private_key.pubkey()
    }

    pub fn get_peer_id(&self) -> PeerId {
        self.private_key.pubkey().tentacle_peer_id()
    }

    pub fn get_node_address(&self) -> &MultiAddr {
        &self.listening_addrs[0]
    }

    pub fn get_local_balance_from_channel(&self, channel_id: Hash256) -> u128 {
        self.store
            .get_channel_actor_state(&channel_id)
            .expect("get channel")
            .to_local_amount
    }

    pub fn get_remote_balance_from_channel(&self, channel_id: Hash256) -> u128 {
        self.store
            .get_channel_actor_state(&channel_id)
            .expect("get channel")
            .to_remote_amount
    }

    pub fn get_tlc(&self, channel_id: Hash256, tlc_id: TLCId) -> Option<TlcInfo> {
        let state = self.get_channel_actor_state(channel_id);
        match tlc_id {
            TLCId::Offered(..) => state.get_offered_tlc(tlc_id).cloned(),
            TLCId::Received(..) => state.get_received_tlc(tlc_id).cloned(),
        }
    }

    pub fn get_channel_actor_state(&self, channel_id: Hash256) -> ChannelActorState {
        self.get_channel_actor_state_unchecked(channel_id)
            .expect("get channel")
    }

    pub fn get_channel_actor_state_unchecked(
        &self,
        channel_id: Hash256,
    ) -> Option<ChannelActorState> {
        self.store.get_channel_actor_state(&channel_id)
    }

    pub fn insert_invoice(&mut self, invoice: CkbInvoice, preimage: Option<Hash256>) {
        self.store
            .insert_invoice(invoice, preimage)
            .expect("insert success");
    }

    pub fn get_invoice_status(&mut self, payment_hash: &Hash256) -> Option<CkbInvoiceStatus> {
        self.store.get_invoice_status(payment_hash)
    }

    pub fn cancel_invoice(&mut self, payment_hash: &Hash256) {
        self.store
            .update_invoice_status(payment_hash, CkbInvoiceStatus::Cancelled)
            .expect("cancel success");
    }

    pub fn get_payment_preimage(&self, payment_hash: &Hash256) -> Option<Hash256> {
        self.store.get_preimage(payment_hash)
    }

    pub async fn send_payment(
        &self,
        command: SendPaymentCommand,
    ) -> Result<SendPaymentResponse, String> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(command, rpc_reply))
        };

        call!(self.network_actor, message).expect("source_node alive")
    }

    pub async fn send_mpp_payment(
        &self,
        target_node: &mut NetworkNode,
        amount: u128,
        max_parts: Option<u64>,
    ) -> Result<SendPaymentResponse, String> {
        self.send_mpp_payment_with_command(
            target_node,
            amount,
            SendPaymentCommand {
                max_parts,
                dry_run: false,
                ..Default::default()
            },
        )
        .await
    }

    pub async fn send_mpp_payment_with_dry_run_option(
        &self,
        target_node: &mut NetworkNode,
        amount: u128,
        max_parts: Option<u64>,
        dry_run: bool,
    ) -> Result<SendPaymentResponse, String> {
        self.send_mpp_payment_with_command(
            target_node,
            amount,
            SendPaymentCommand {
                max_parts,
                dry_run,
                ..Default::default()
            },
        )
        .await
    }

    pub async fn send_mpp_payment_with_command(
        &self,
        target_node: &mut NetworkNode,
        amount: u128,
        command: SendPaymentCommand,
    ) -> Result<SendPaymentResponse, String> {
        let target_pubkey = target_node.get_public_key();
        let preimage = gen_rand_sha256_hash();
        let ckb_invoice = InvoiceBuilder::new(Currency::Fibd)
            .amount(Some(amount))
            .payment_preimage(preimage)
            .payee_pub_key(target_pubkey.into())
            .allow_mpp(true)
            .payment_secret(gen_rand_sha256_hash())
            .build()
            .expect("build invoice success");

        target_node.insert_invoice(ckb_invoice.clone(), Some(preimage));
        let mut command = command.clone();
        command.invoice = Some(ckb_invoice.to_string());

        self.send_payment(command).await
    }

    pub async fn assert_send_payment_success(
        &self,
        command: SendPaymentCommand,
    ) -> SendPaymentResponse {
        let res = self.send_payment(command).await;
        assert!(res.is_ok());
        let res = res.unwrap();
        self.wait_until_success(res.payment_hash).await;
        res
    }

    pub async fn send_payment_with_router(
        &self,
        command: SendPaymentWithRouterCommand,
    ) -> Result<SendPaymentResponse, String> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPaymentWithRouter(
                command, rpc_reply,
            ))
        };

        call!(self.network_actor, message).expect("source_node alive")
    }

    pub async fn build_router(&self, command: BuildRouterCommand) -> Result<PaymentRouter, String> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::BuildPaymentRouter(
                command, rpc_reply,
            ))
        };

        call!(self.network_actor, message).expect("source_node alive")
    }

    pub async fn send_abandon_channel(&self, channel_id: Hash256) -> Result<(), String> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::AbandonChannel(channel_id, rpc_reply))
        };
        call!(self.network_actor, message).expect("node_a alive")
    }

    pub async fn send_shutdown(
        &self,
        channel_id: Hash256,
        force: bool,
    ) -> std::result::Result<(), String> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id,
                    command: ChannelCommand::Shutdown(
                        ShutdownCommand {
                            close_script: None,
                            fee_rate: Some(FeeRate::from_u64(1000000000)),
                            force,
                        },
                        rpc_reply,
                    ),
                },
            ))
        };

        call!(self.network_actor, message).expect("source_node alive")
    }

    pub async fn send_channel_shutdown_tx_confirmed_event(
        &self,
        peer_id: PeerId,
        channel_id: Hash256,
        force: bool,
    ) {
        use crate::fiber::NetworkActorEvent::ClosingTransactionConfirmed;

        let tx_hash = TransactionBuilder::default().build().hash();
        let event = ClosingTransactionConfirmed(peer_id, channel_id, tx_hash, force);
        self.network_actor
            .send_message(NetworkActorMessage::Event(event))
            .expect("network actor alive");
    }

    pub async fn send_payment_keysend(
        &self,
        recipient: &NetworkNode,
        amount: u128,
        dry_run: bool,
    ) -> std::result::Result<SendPaymentResponse, String> {
        self.send_payment(SendPaymentCommand {
            target_pubkey: Some(recipient.pubkey),
            amount: Some(amount),
            keysend: Some(true),
            allow_self_payment: false,
            dry_run,
            ..Default::default()
        })
        .await
    }
    
    pub fn set_auth_token(&mut self, token: String) {
        self.auth_token = Some(token);
    }
    
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn send_rpc_request_raw<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> Result<serde_json::Value, String> {
        if let Some((_server, socket_addr)) = &self.rpc_server {
            let mut headers = HeaderMap::new();
            if let Some(token) = &self.auth_token {
                let value = HeaderValue::from_str(format!("Bearer {token}").as_str()).unwrap();
                headers.insert("Authorization", value);
            }
            let client = HttpClient::<HttpBackend>::builder()
                .set_headers(headers)
                .build(format!("http://{}", socket_addr))
                .expect("build client");
            let params = rpc_params![params];
            let response: serde_json::Value = client
                .request(method, params)
                .await
                .map_err(|err| err.to_string())?;
            Self::verify_serde_json_value(response.clone()).expect("verify response");
            Ok(response)
        } else {
            Err("RPC server not started".to_string())
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn send_rpc_request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, String> {
        let response = self.send_rpc_request_raw(method, params).await?;
        let result = serde_json::from_value::<R>(response.clone())
            .map_err(|e| format!("failed to deserialize response: {}", e))?;
        Ok(result)
    }

    // verify serde_json::Value do not contains any Number,
    // we expect all number values are hex encoded
    pub fn verify_serde_json_value(value: serde_json::Value) -> Result<(), String> {
        if value.is_array() {
            for val in value.as_array().unwrap() {
                if val.is_object() || val.is_array() {
                    Self::verify_serde_json_value(val.clone())?;
                }
            }
        }
        if value.is_object() {
            for (key, val) in value.as_object().unwrap() {
                if val.is_number() {
                    return Err(format!(
                        "field should be in hex encoded: {}, but it is with value: {}",
                        key, val,
                    ));
                }
                if val.is_object() || val.is_array() {
                    Self::verify_serde_json_value(val.clone())?;
                }
            }
        }
        Ok(())
    }

    pub async fn gen_invoice(&self, new_invoice_params: NewInvoiceParams) -> InvoiceResult {
        let invoice: InvoiceResult = self
            .send_rpc_request("new_invoice", new_invoice_params)
            .await
            .unwrap();

        invoice
    }

    pub async fn send_payment_keysend_to_self(
        &self,
        amount: u128,
        dry_run: bool,
    ) -> std::result::Result<SendPaymentResponse, String> {
        let pubkey = self.pubkey;
        self.send_payment(SendPaymentCommand {
            target_pubkey: Some(pubkey),
            amount: Some(amount),
            keysend: Some(true),
            allow_self_payment: true,
            dry_run,
            ..Default::default()
        })
        .await
    }

    pub async fn assert_payment_status(
        &self,
        payment_hash: Hash256,
        expected_status: PaymentStatus,
        expected_retried: Option<u32>,
    ) {
        let status = self.get_payment_status(payment_hash).await;
        assert_eq!(status, expected_status);

        if let Some(expected_retried) = expected_retried {
            let payment_session = self.get_payment_session(payment_hash).unwrap();
            assert_eq!(payment_session.retry_times(), expected_retried);
        }
    }

    pub async fn get_payment_status(&self, payment_hash: Hash256) -> PaymentStatus {
        self.get_payment_result(payment_hash).await.status
    }

    pub async fn get_payment_result(&self, payment_hash: Hash256) -> SendPaymentResponse {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, rpc_reply))
        };
        call!(self.network_actor, message)
            .expect("node_a alive")
            .unwrap()
    }

    pub async fn expect_payment_used_channel(&self, payment_hash: Hash256, channel_id: Hash256) {
        let payment_result = self.get_payment_result(payment_hash).await;
        self.expect_router_used_channel(&payment_result, channel_id)
            .await;
    }

    pub async fn expect_router_used_channel(
        &self,
        payment_result: &SendPaymentResponse,
        channel_id: Hash256,
    ) {
        let used_channels = payment_result.routers[0]
            .nodes
            .iter()
            .map(|r| r.channel_outpoint.clone())
            .collect::<Vec<_>>();
        let funding_tx = self
            .get_channel_funding_tx(&channel_id)
            .expect("funding tx");
        let channel_outpoint = OutPoint::new(funding_tx.into(), 0);
        assert!(used_channels.contains(&channel_outpoint));
    }

    pub async fn routers_used_channels(
        &self,
        routers: &[SessionRoute],
        channel_ids: &[Hash256],
    ) -> Vec<Hash256> {
        let mut routers_channel_outpoints = vec![];
        for route in routers {
            let channel_outpoints = route
                .nodes
                .iter()
                .map(|r| r.channel_outpoint.clone())
                .collect::<Vec<_>>();
            for outpoint in channel_outpoints {
                routers_channel_outpoints.push(outpoint.clone());
            }
        }

        channel_ids
            .iter()
            .filter(|id| {
                let funding_tx = self.get_channel_funding_tx(id).expect("funding tx");
                let channel_outpoint = OutPoint::new(funding_tx.into(), 0);
                routers_channel_outpoints.contains(&channel_outpoint)
            })
            .copied()
            .collect::<Vec<_>>()
    }

    async fn wait_until_status<F, E>(
        &self,
        payment_hash: Hash256,
        check: F,
        on_unexpected: E,
        err_msg: &str,
    ) where
        F: Fn(PaymentStatus) -> bool,
        E: Fn(PaymentStatus),
    {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(DEFAULT_WAIT_UNTIL_TIME) {
            assert!(self.get_triggered_unexpected_events().await.is_empty());
            let status = self.get_payment_status(payment_hash).await;
            if check(status) {
                return;
            }
            on_unexpected(status);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!(
            "{}: {:?}, current status: {:?}",
            err_msg,
            payment_hash,
            self.get_payment_status(payment_hash).await
        );
    }

    pub async fn wait_until_success(&self, payment_hash: Hash256) {
        self.wait_until_status(
            payment_hash,
            |status| status == PaymentStatus::Success,
            |status| {
                if status == PaymentStatus::Failed {
                    error!("Unexpected payment failed: {:?}\n\n", payment_hash);
                    assert_eq!(status, PaymentStatus::Success);
                }
            },
            "Payment did not succeed within the expected time",
        )
        .await;
    }

    pub async fn wait_until_failed(&self, payment_hash: Hash256) {
        self.wait_until_status(
            payment_hash,
            |status| status == PaymentStatus::Failed,
            |status| {
                if status == PaymentStatus::Success {
                    error!("Payment success: {:?}\n\n", payment_hash);
                    assert_eq!(status, PaymentStatus::Failed);
                }
            },
            "Payment did not fail within the expected time",
        )
        .await;
    }

    pub async fn wait_until_created(&self, payment_hash: Hash256) {
        self.wait_until_status(
            payment_hash,
            |status| status != PaymentStatus::Created,
            |_status| {},
            "Payment did not reach the created status within the expected time",
        )
        .await;
    }

    pub async fn wait_until_inflight(&self, payment_hash: Hash256) {
        self.wait_until_status(
            payment_hash,
            |status| status == PaymentStatus::Inflight,
            |_status| {},
            "Payment did not reach in-flight status within the expected time",
        )
        .await;
    }

    pub async fn wait_until_final_status(&self, payment_hash: Hash256) {
        self.wait_until_status(
            payment_hash,
            |status| matches!(status, PaymentStatus::Success | PaymentStatus::Failed),
            |_status| {},
            "Payment did not reach final status within the expected time",
        )
        .await;
    }

    pub async fn node_info(&self) -> NodeInfoResponse {
        let message =
            |rpc_reply| NetworkActorMessage::Command(NetworkActorCommand::NodeInfo((), rpc_reply));

        call!(self.network_actor, message)
            .expect("node_a alive")
            .unwrap()
    }

    pub async fn update_channel_actor_state(
        &self,
        state: ChannelActorState,
        reload_params: Option<ReloadParams>,
    ) {
        let channel_id = state.id;
        self.store.insert_channel_actor_state(state);
        self.network_actor
            .send_message(NetworkActorMessage::Command(
                NetworkActorCommand::ControlFiberChannel(ChannelCommandWithId {
                    channel_id,
                    command: ChannelCommand::ReloadState(reload_params.unwrap_or_default()),
                }),
            ))
            .expect("network actor is live");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    pub async fn update_channel_local_balance(
        &self,
        channel_id: Hash256,
        new_to_local_amount: u128,
    ) {
        let mut channel_actor_state = self.get_channel_actor_state(channel_id);
        channel_actor_state.to_local_amount = new_to_local_amount;
        self.update_channel_actor_state(channel_actor_state, None)
            .await;
    }

    pub async fn update_channel_remote_balance(
        &self,
        channel_id: Hash256,
        new_to_remote_amount: u128,
    ) {
        let mut channel_actor_state = self.get_channel_actor_state(channel_id);
        channel_actor_state.to_remote_amount = new_to_remote_amount;
        self.update_channel_actor_state(channel_actor_state, None)
            .await;
    }

    pub async fn disable_channel(&mut self, channel_id: Hash256) {
        let mut channel_actor_state = self.get_channel_actor_state(channel_id);
        channel_actor_state.local_tlc_info.enabled = false;
        self.update_channel_actor_state(channel_actor_state, None)
            .await;
    }

    pub async fn disable_channel_stealthy(&self, channel_id: Hash256) {
        let mut channel_actor_state = self.get_channel_actor_state(channel_id);
        channel_actor_state.local_tlc_info.enabled = false;
        self.update_channel_actor_state(
            channel_actor_state,
            Some(ReloadParams {
                notify_changes: false,
            }),
        )
        .await;
    }

    pub async fn handle_shutdown_command_without_check(
        &self,
        channel_id: Hash256,
        command: ShutdownCommand,
    ) {
        let state = self.get_channel_actor_state(channel_id);
        self.network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                    state.get_remote_peer_id(),
                    FiberMessage::shutdown(Shutdown {
                        channel_id: state.get_id(),
                        close_script: command
                            .close_script
                            .clone()
                            .unwrap_or(state.local_shutdown_script),
                        fee_rate: command
                            .fee_rate
                            .unwrap_or(FeeRate::from_u64(state.commitment_fee_rate)),
                    }),
                )),
            ))
            .expect(ASSUME_NETWORK_ACTOR_ALIVE);
    }

    pub async fn update_channel_with_command(&self, channel_id: Hash256, command: UpdateCommand) {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::ControlFiberChannel(
                ChannelCommandWithId {
                    channel_id,
                    command: ChannelCommand::Update(command, rpc_reply),
                },
            ))
        };
        call!(self.network_actor, message)
            .expect("node_a alive")
            .expect("update channel success");
    }

    pub async fn update_node_features(&self, features: FeatureVector) {
        let message = NetworkActorMessage::Command(NetworkActorCommand::UpdateFeatures(features));

        self.network_actor
            .send_message(message)
            .expect("network actor is live");
    }

    pub fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession> {
        self.store.get_payment_session(payment_hash)
    }

    pub fn assert_router_used(&self, index: usize, payment_hash: Hash256, channel_id: Hash256) {
        let funding_tx_hash = self.get_channel_funding_tx(&channel_id).unwrap();
        let channel_outpoint = OutPoint::new(funding_tx_hash.into(), 0);
        let payment_session = self.get_payment_session(payment_hash).unwrap();
        let first_attempt = payment_session
            .attempts()
            .next()
            .expect("at least one attempt");
        assert_eq!(
            first_attempt.route.nodes[index].channel_outpoint,
            channel_outpoint
        );
    }

    pub fn get_payment_custom_records(
        &self,
        payment_hash: &Hash256,
    ) -> Option<PaymentCustomRecords> {
        self.store.get_payment_custom_records(payment_hash)
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
            ckb_config,
            rpc_config,
            mock_chain_actor_middleware,
        } = config;

        let _span = tracing::info_span!("NetworkNode", node_name = &node_name).entered();

        let root = get_test_root_actor().await;
        let (event_sender, mut event_receiver) = mpsc::channel(10000);

        let chain_actor = Actor::spawn_linked(
            None,
            MockChainActor::new(),
            mock_chain_actor_middleware.clone(),
            root.get_cell(),
        )
        .await
        .expect("start mock chain actor")
        .0;

        let private_key: Privkey = fiber_config
            .read_or_generate_secret_key()
            .expect("must generate key")
            .into();
        let pubkey = private_key.pubkey();

        let network_graph = Arc::new(TokioRwLock::new(NetworkGraph::new(
            store.clone(),
            pubkey,
            true,
        )));

        let network_actor = Actor::spawn_linked(
            Some(format!("network actor at {}", base_dir.to_str())),
            NetworkActor::new(
                event_sender,
                chain_actor.clone(),
                store.clone(),
                network_graph.clone(),
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

        let mut unexpected_events: HashSet<String> = HashSet::new();

        // Some usual unexpected events that we want to not happened
        // use `assert!(node.get_triggered_unexpected_events().await.is_empty())` to check it
        let default_unexpected_events = vec![
            "Musig2VerifyError",
            "Musig2RoundFinalizeError",
            "InvalidOnionError",
        ];
        for event in default_unexpected_events {
            unexpected_events.insert(event.to_string());
        }

        let unexpected_events = Arc::new(TokioRwLock::new(unexpected_events));
        let triggered_unexpected_events = Arc::new(TokioRwLock::new(Vec::<String>::new()));
        let (self_event_sender, self_event_receiver) = mpsc::channel(10000);
        let unexpected_events_clone = unexpected_events.clone();
        let triggered_unexpected_events_clone = triggered_unexpected_events.clone();
        // spawn a new thread to collect all the events from event_receiver
        tokio::spawn(async move {
            while let Some(event) = event_receiver.recv().await {
                self_event_sender
                    .send(event.clone())
                    .await
                    .expect("send event");
                let unexpected_events = unexpected_events_clone.read().await;
                let event_content = format!("{:?}", event);
                for unexpected_event in unexpected_events.iter() {
                    if event_content.contains(unexpected_event) {
                        triggered_unexpected_events_clone
                            .write()
                            .await
                            .push(unexpected_event.clone());
                    }
                }
            }
        });

        info!(
            "Network node started for peer_id {:?} in directory {:?}",
            &peer_id,
            base_dir.as_ref()
        );

        let gossip_actor = ractor::registry::where_is(get_gossip_actor_name(&peer_id))
            .expect("gossip actor should have been started")
            .into();
        #[cfg(not(target_arch = "wasm32"))]
        let rpc_server = if let Some(rpc_config) = rpc_config.clone() {
            Some(
                start_rpc(
                    rpc_config,
                    ckb_config.clone(),
                    Some(fiber_config.clone()),
                    Some(network_actor.clone()),
                    None,
                    store.clone(),
                    network_graph.clone(),
                    #[cfg(debug_assertions)]
                    None,
                    #[cfg(debug_assertions)]
                    None,
                )
                .await
                .unwrap(),
            )
        } else {
            None
        };

        Self {
            base_dir,
            node_name,
            store,
            fiber_config,
            ckb_config,
            rpc_config,
            channels_tx_map: Default::default(),
            listening_addrs: announced_addrs,
            network_actor,
            ckb_chain_actor: chain_actor.clone(),
            mock_chain_actor_middleware,
            network_graph,
            chain_actor,
            gossip_actor,
            private_key,
            peer_id,
            event_emitter: self_event_receiver,
            pubkey,
            unexpected_events,
            triggered_unexpected_events,
            #[cfg(not(target_arch = "wasm32"))]
            rpc_server,
            auth_token: None,
        }
    }

    pub fn get_node_config(&self) -> NetworkNodeConfig {
        NetworkNodeConfig {
            base_dir: self.base_dir.clone(),
            node_name: self.node_name.clone(),
            store: self.store.clone(),
            ckb_config: self.ckb_config.clone(),
            fiber_config: self.fiber_config.clone(),
            rpc_config: self.rpc_config.clone(),
            mock_chain_actor_middleware: self.mock_chain_actor_middleware.clone(),
        }
    }

    pub fn send_ckb_chain_message(&self, message: CkbChainMessage) {
        self.ckb_chain_actor
            .send_message(message)
            .expect("send ckb chain message");
    }

    pub fn send_init_peer_message(&self, remote_peer_id: PeerId, message: Init) {
        self.network_actor
            .send_message(NetworkActorMessage::new_command(
                crate::fiber::NetworkActorCommand::SendFiberMessage(FiberMessageWithPeerId::new(
                    remote_peer_id,
                    FiberMessage::Init(message),
                )),
            ))
            .expect("send init peer message");
    }

    pub async fn add_unexpected_events(&self, events: Vec<String>) {
        let mut unexpected_events = self.unexpected_events.write().await;
        for event in events {
            unexpected_events.insert(event);
        }
    }

    pub async fn get_triggered_unexpected_events(&self) -> Vec<String> {
        self.triggered_unexpected_events.read().await.clone()
    }

    pub async fn get_network_channels(&self) -> Vec<ChannelInfo> {
        self.network_graph
            .read()
            .await
            .get_channels_with_params(1000, None)
    }

    pub async fn get_network_nodes(&self) -> Vec<NodeInfo> {
        self.network_graph
            .read()
            .await
            .get_nodes_with_params(1000, None)
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
        let nodes = Self::new_interconnected_nodes(N, None).await;
        match nodes.try_into() {
            Ok(nodes) => nodes,
            Err(_) => unreachable!(),
        }
    }

    pub async fn new_interconnected_nodes(n: usize, rpc_config: Option<RpcConfig>) -> Vec<Self> {
        let mut nodes: Vec<NetworkNode> = Vec::with_capacity(n);
        for i in 0..n {
            let mut new = Self::new_with_config(
                NetworkNodeConfigBuilder::new()
                    .node_name(Some(format!("node-{}", i)))
                    .base_dir_prefix(&format!("test-fnn-node-{}-", i))
                    .rpc_config(rpc_config.clone())
                    .build(),
            )
            .await;
            for node in nodes.iter_mut() {
                node.connect_to(&mut new).await;
            }
            nodes.push(new);
        }
        #[allow(clippy::useless_conversion)]
        match nodes.try_into() {
            Ok(nodes) => nodes,
            Err(_) => unreachable!(),
        }
    }

    pub async fn new_2_nodes_with_established_channel(
        node_a_funding_amount: u128,
        node_b_funding_amount: u128,
        public: bool,
    ) -> (NetworkNode, NetworkNode, Hash256, TransactionView) {
        let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

        let (channel_id, funding_tx_hash) = establish_channel_between_nodes(
            &mut node_a,
            &mut node_b,
            ChannelParameters {
                public,
                node_a_funding_amount,
                node_b_funding_amount,
                ..Default::default()
            },
        )
        .await;
        let funding_tx = node_a
            .get_transaction_view_from_hash(funding_tx_hash)
            .await
            .expect("get funding tx");

        (node_a, node_b, channel_id, funding_tx)
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
            let mut new = Self::new_with_config(config_gen(i)).await;
            for node in nodes.iter_mut() {
                node.connect_to(&mut new).await;
            }
            nodes.push(new);
        }
        nodes
    }

    pub async fn connect_to_nonblocking(&mut self, other: &Self) {
        let peer_addr = other.listening_addrs[0].clone();
        debug!(
            "Trying to connect to {:?} from {:?}",
            other.listening_addrs, &self.listening_addrs
        );

        self.network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::ConnectPeer(peer_addr.clone()),
            ))
            .expect("self alive");
    }

    pub async fn connect_to(&mut self, other: &mut Self) {
        self.connect_to_nonblocking(other).await;
        let peer_id = &other.peer_id;
        self.expect_event(
            |event| matches!(event, NetworkServiceEvent::PeerConnected(id, _addr) if id == peer_id),
        )
        .await;
        self.expect_debug_event("PeerInit").await;
        other.expect_debug_event("PeerInit").await;
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
                            debug!("Received event when waiting for specific event: {:?}", &event);
                            if let Some(r) = event_processor(&event) {
                                debug!("Event ({:?}) matching filter received, exiting waiting for event loop", &event);
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

    pub async fn expect_debug_event(&mut self, message: &str) {
        self.expect_event(|event| {
            matches!(event, NetworkServiceEvent::DebugEvent(DebugEvent::Common(msg)) if msg == message)
        })
        .await;
    }

    pub async fn submit_tx(&self, tx: TransactionView) -> TxStatus {
        submit_tx(self.chain_actor.clone(), tx).await
    }

    pub fn add_channel_tx(&mut self, channel_id: Hash256, tx_hash: Hash256) {
        self.channels_tx_map.insert(channel_id, tx_hash);
    }

    pub fn get_channel_funding_tx(&self, channel_id: &Hash256) -> Option<Hash256> {
        self.channels_tx_map.get(channel_id).cloned()
    }

    pub fn get_channel_outpoint(&self, channel_id: &Hash256) -> Option<OutPoint> {
        self.get_channel_funding_tx(channel_id)
            .map(|funding_tx| OutPoint::new(funding_tx.into(), 0))
    }

    pub async fn trace_tx(&mut self, tx_hash: Hash256) -> TxStatus {
        trace_tx(self.chain_actor.clone(), tx_hash).await
    }

    pub async fn get_tx_from_hash(
        &mut self,
        tx_hash: Hash256,
    ) -> Result<GetTxResponse, anyhow::Error> {
        get_tx_from_hash(self.chain_actor.clone(), tx_hash)
            .await
            .map_err(Into::into)
    }

    pub async fn get_transaction_view_from_hash(
        &mut self,
        tx_hash: Hash256,
    ) -> Option<TransactionView> {
        self.get_tx_from_hash(tx_hash)
            .await
            .ok()
            .and_then(|response| response.transaction)
    }

    pub fn get_network_graph(&self) -> &Arc<TokioRwLock<NetworkGraph<Store>>> {
        &self.network_graph
    }

    pub async fn clear_history(&self) {
        self.network_graph.write().await.clear_history();
    }

    pub async fn with_network_graph<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&NetworkGraph<Store>) -> T,
    {
        let graph = self.get_network_graph().read().await;
        f(&graph)
    }

    pub async fn with_network_graph_mut<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut NetworkGraph<Store>) -> T,
    {
        let mut graph = self.get_network_graph().write().await;
        f(&mut graph)
    }

    pub async fn get_network_graph_nodes(&self) -> Vec<NodeInfo> {
        self.with_network_graph(|graph| graph.nodes().cloned().collect())
            .await
    }

    pub async fn get_network_graph_node(&self, pubkey: &Pubkey) -> Option<NodeInfo> {
        self.with_network_graph(|graph| graph.get_node(pubkey).cloned())
            .await
    }

    pub async fn get_network_graph_channels(&self) -> Vec<ChannelInfo> {
        self.with_network_graph(|graph| graph.channels().cloned().collect())
            .await
    }

    pub async fn get_network_graph_channel(&self, channel_id: &OutPoint) -> Option<ChannelInfo> {
        self.with_network_graph(|graph| {
            tracing::debug!("Getting channel info for {:?}", channel_id);
            tracing::debug!("Channels: {:?}", graph.channels().collect::<Vec<_>>());
            graph.get_channel(channel_id).cloned()
        })
        .await
    }

    pub fn send_message_to_gossip_actor(&self, message: GossipActorMessage) {
        self.gossip_actor
            .send_message(message)
            .expect("send message to gossip actor");
    }

    pub fn mock_received_gossip_message_from_peer(&self, peer_id: PeerId, message: GossipMessage) {
        self.send_message_to_gossip_actor(GossipActorMessage::GossipMessageReceived(
            GossipMessageWithPeerId { peer_id, message },
        ));
    }

    pub fn get_store(&self) -> &Store {
        &self.store
    }
}

pub async fn create_mock_chain_actor() -> ActorRef<CkbChainMessage> {
    Actor::spawn(None, MockChainActor::new(), None)
        .await
        .expect("start mock chain actor")
        .0
}

pub async fn wait_for_network_graph_update(node: &NetworkNode, channels: usize) {
    // sleep for a while to make sure network graph is updated
    for _ in 0..50 {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if node.get_network_graph_channels().await.len() >= channels {
            break;
        }
    }
    let graph_channels = node.get_network_graph_channels().await;
    if graph_channels.len() < channels {
        use tracing::error;
        error!(
            "failed to sync all graph channels, expect {} got {}",
            channels,
            graph_channels.len()
        );
        for (i, chan) in graph_channels.into_iter().enumerate() {
            error!(">>> channel {}: {:?}", i, chan);
        }
    }
}

pub async fn wait_until_timeout<F: Fn() -> bool>(max_wait_time: u64, f: F) {
    let start = tokio::time::Instant::now();
    while !f() {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if start.elapsed().as_millis() > max_wait_time as u128 {
            panic!("Wait timeout after {}ms", max_wait_time);
        }
    }
}

pub async fn wait_until<F: Fn() -> bool>(f: F) {
    const MAX_WAIT_TIME: u64 = 120_000;

    wait_until_timeout(MAX_WAIT_TIME, f).await;
}

#[tokio::test]
async fn test_connect_to_other_node() {
    let mut node_a = NetworkNode::new().await;
    let mut node_b = NetworkNode::new().await;
    node_a.connect_to(&mut node_b).await;
}

#[tokio::test]
async fn test_restart_network_node() {
    let mut node = NetworkNode::new().await;
    node.restart().await;
}
