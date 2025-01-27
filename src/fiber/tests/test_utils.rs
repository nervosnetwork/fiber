use crate::fiber::channel::ChannelActorState;
use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber::channel::ChannelCommand;
use crate::fiber::channel::ChannelCommandWithId;
use crate::fiber::channel::ReloadParams;
use crate::fiber::channel::UpdateCommand;
use crate::fiber::graph::NetworkGraphStateStore;
use crate::fiber::graph::PaymentSession;
use crate::fiber::graph::PaymentSessionStatus;
use crate::fiber::network::NodeInfoResponse;
use crate::fiber::network::SendPaymentCommand;
use crate::fiber::network::SendPaymentResponse;
use crate::fiber::types::EcdsaSignature;
use crate::fiber::types::Pubkey;
use crate::invoice::CkbInvoice;
use crate::invoice::CkbInvoiceStatus;
use crate::invoice::InvoiceStore;
use ckb_jsonrpc_types::Status;
use ckb_types::packed::OutPoint;
use ckb_types::{core::TransactionView, packed::Byte32};
use ractor::{call, Actor, ActorRef};
use rand::rngs::OsRng;
use secp256k1::{Message, Secp256k1};
use std::collections::HashMap;
use std::collections::HashSet;
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

use crate::fiber::graph::ChannelInfo;
use crate::fiber::graph::NodeInfo;
use crate::fiber::network::{AcceptChannelCommand, OpenChannelCommand};
use crate::fiber::types::Privkey;
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

static RETAIN_VAR: &str = "TEST_TEMP_RETAIN";
pub(crate) const MIN_RESERVED_CKB: u128 = 4200000000;
pub(crate) const HUGE_CKB_AMOUNT: u128 = MIN_RESERVED_CKB + 1000000000000 as u128;

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
        // This config is needed for the timely processing of gossip messages.
        // Without this, some tests may fail due to the delay in processing gossip messages.
        gossip_network_maintenance_interval_ms: Some(50),
        // This config is needed for the timely processing of gossip messages.
        // Without this, some tests may fail due to the delay in processing gossip messages.
        gossip_store_maintenance_interval_ms: Some(50),
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
    let temp_dir = TempDir::new("test-fnn-node");
    let store = Store::new(temp_dir.as_ref());
    store.expect("create store")
}

#[derive(Debug)]
pub struct NetworkNode {
    /// The base directory of the node, will be deleted after this struct dropped.
    pub base_dir: Arc<TempDir>,
    pub node_name: Option<String>,
    pub store: Store,
    pub channels_tx_map: HashMap<Hash256, Hash256>,
    pub fiber_config: FiberConfig,
    pub listening_addrs: Vec<MultiAddr>,
    pub network_actor: ActorRef<NetworkActorMessage>,
    pub network_graph: Arc<TokioRwLock<NetworkGraph<Store>>>,
    pub chain_actor: ActorRef<CkbChainMessage>,
    pub private_key: Privkey,
    pub peer_id: PeerId,
    pub event_emitter: mpsc::Receiver<NetworkServiceEvent>,
    pub pubkey: Pubkey,
    pub unexpected_events: Arc<TokioRwLock<HashSet<String>>>,
    pub triggered_unexpected_events: Arc<TokioRwLock<Vec<String>>>,
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
            .unwrap_or_else(|| Arc::new(TempDir::new("test-fnn-node")));
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

pub(crate) async fn establish_channel_between_nodes(
    node_a: &mut NetworkNode,
    node_b: &mut NetworkNode,
    public: bool,
    node_a_funding_amount: u128,
    node_b_funding_amount: u128,
    a_max_tlc_number_in_flight: Option<u64>,
    a_max_tlc_value_in_flight: Option<u128>,
    a_tlc_expiry_delta: Option<u64>,
    a_tlc_min_value: Option<u128>,
    a_tlc_fee_proportional_millionths: Option<u128>,
    b_max_tlc_number_in_flight: Option<u64>,
    b_max_tlc_value_in_flight: Option<u128>,
    b_tlc_expiry_delta: Option<u64>,
    b_tlc_min_value: Option<u128>,
    b_tlc_fee_proportional_millionths: Option<u128>,
) -> (Hash256, TransactionView) {
    let message = |rpc_reply| {
        NetworkActorMessage::Command(NetworkActorCommand::OpenChannel(
            OpenChannelCommand {
                peer_id: node_b.peer_id.clone(),
                public,
                shutdown_script: None,
                funding_amount: node_a_funding_amount,
                funding_udt_type_script: None,
                commitment_fee_rate: None,
                commitment_delay_epoch: None,
                funding_fee_rate: None,
                tlc_expiry_delta: a_tlc_expiry_delta,
                tlc_min_value: a_tlc_min_value,
                tlc_fee_proportional_millionths: a_tlc_fee_proportional_millionths,
                max_tlc_number_in_flight: a_max_tlc_number_in_flight,
                max_tlc_value_in_flight: a_max_tlc_value_in_flight,
            },
            rpc_reply,
        ))
    };
    let open_channel_result = call!(node_a.network_actor, message)
        .expect("node_a alive")
        .expect("open channel success");

    node_b
        .expect_event(|event| match event {
            NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, channel_id) => {
                println!("A channel ({:?}) to {:?} create", &channel_id, peer_id);
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
                funding_amount: node_b_funding_amount,
                shutdown_script: None,
                max_tlc_number_in_flight: b_max_tlc_number_in_flight,
                max_tlc_value_in_flight: b_max_tlc_value_in_flight,
                min_tlc_value: b_tlc_min_value,
                tlc_fee_proportional_millionths: b_tlc_fee_proportional_millionths,
                tlc_expiry_delta: b_tlc_expiry_delta,
            },
            rpc_reply,
        ))
    };
    let accept_channel_result = call!(node_b.network_actor, message)
        .expect("node_b alive")
        .expect("accept channel success");
    let new_channel_id = accept_channel_result.new_channel_id;

    let funding_tx_outpoint = node_a
        .expect_to_process_event(|event| match event {
            NetworkServiceEvent::ChannelReady(peer_id, channel_id, funding_tx_outpoint) => {
                println!(
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
                println!(
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

    let funding_tx = node_a
        .get_tx_from_hash(funding_tx_outpoint.tx_hash())
        .await
        .expect("tx found");

    node_a.add_channel_tx(new_channel_id, funding_tx.clone());
    node_b.add_channel_tx(new_channel_id, funding_tx.clone());

    (new_channel_id, funding_tx)
}

pub(crate) async fn create_nodes_with_established_channel(
    node_a_funding_amount: u128,
    node_b_funding_amount: u128,
    public: bool,
) -> (NetworkNode, NetworkNode, Hash256) {
    let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

    let (channel_id, _funding_tx) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        public,
        node_a_funding_amount,
        node_b_funding_amount,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    (node_a, node_b, channel_id)
}

pub(crate) async fn create_3_nodes_with_established_channel(
    (channel_1_amount_a, channel_1_amount_b): (u128, u128),
    (channel_2_amount_b, channel_2_amount_c): (u128, u128),
    public: bool,
) -> (NetworkNode, NetworkNode, NetworkNode, Hash256, Hash256) {
    let (nodes, channels) = create_n_nodes_with_established_channel(
        &[
            (channel_1_amount_a, channel_1_amount_b),
            (channel_2_amount_b, channel_2_amount_c),
        ],
        3,
        public,
    )
    .await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");
    (node_a, node_b, node_c, channels[0], channels[1])
}

// make a network like A -> B -> C -> D
pub(crate) async fn create_n_nodes_with_established_channel(
    amounts: &[(u128, u128)],
    n: usize,
    public: bool,
) -> (Vec<NetworkNode>, Vec<Hash256>) {
    assert!(n >= 2);
    assert_eq!(amounts.len(), n - 1);

    let nodes_index_map: Vec<((usize, usize), (u128, u128))> = (0..n - 1)
        .map(|i| ((i, i + 1), (amounts[i].0, amounts[i].1)))
        .collect();

    create_n_nodes_with_index_and_amounts_with_established_channel(&nodes_index_map, n, public)
        .await
}

pub(crate) async fn create_n_nodes_with_index_and_amounts_with_established_channel(
    amounts: &[((usize, usize), (u128, u128))],
    n: usize,
    public: bool,
) -> (Vec<NetworkNode>, Vec<Hash256>) {
    assert!(n >= 2);
    let mut nodes = NetworkNode::new_interconnected_nodes(n).await;
    let mut channels = vec![];

    for &((i, j), (node_a_amount, node_b_amount)) in amounts.iter() {
        let (channel_id, funding_tx) = {
            let (node_a, node_b) = {
                // avoid borrow nodes as mutbale more than once
                assert_ne!(i, j);
                if i < j {
                    let (left, right) = nodes.split_at_mut(i + 1);
                    (&mut left[i], &mut right[j - i - 1])
                } else {
                    let (left, right) = nodes.split_at_mut(j + 1);
                    (&mut right[i - j - 1], &mut left[j])
                }
            };
            establish_channel_between_nodes(
                node_a,
                node_b,
                public,
                node_a_amount,
                node_b_amount,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
        };
        channels.push(channel_id);
        // all the other nodes submit_tx
        for k in 0..n {
            let res = nodes[k].submit_tx(funding_tx.clone()).await;
            nodes[k].add_channel_tx(channel_id, funding_tx.clone());
            assert_eq!(res, Status::Committed);
        }
    }
    // sleep for a while to make sure network graph is updated
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    (nodes, channels)
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

    pub fn get_channel_actor_state(&self, channel_id: Hash256) -> ChannelActorState {
        self.store
            .get_channel_actor_state(&channel_id)
            .expect("get channel")
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
            .expect("cancell success");
    }

    pub async fn send_payment(
        &self,
        command: SendPaymentCommand,
    ) -> std::result::Result<SendPaymentResponse, String> {
        let message = |rpc_reply| -> NetworkActorMessage {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(command, rpc_reply))
        };

        let res = call!(self.network_actor, message).expect("source_node alive");
        eprintln!("result: {:?}", res);
        res
    }

    pub async fn send_payment_keysend(
        &self,
        recipient: &NetworkNode,
        amount: u128,
        dry_run: bool,
    ) -> std::result::Result<SendPaymentResponse, String> {
        self.send_payment(SendPaymentCommand {
            target_pubkey: Some(recipient.pubkey.clone()),
            amount: Some(amount),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: false,
            dry_run,
            hop_hints: None,
        })
        .await
    }

    pub async fn send_payment_keysend_to_self(
        &self,
        amount: u128,
        dry_run: bool,
    ) -> std::result::Result<SendPaymentResponse, String> {
        let pubkey = self.pubkey.clone();
        self.send_payment(SendPaymentCommand {
            target_pubkey: Some(pubkey),
            amount: Some(amount),
            payment_hash: None,
            final_tlc_expiry_delta: None,
            tlc_expiry_limit: None,
            invoice: None,
            timeout: None,
            max_fee_amount: None,
            max_parts: None,
            keysend: Some(true),
            udt_type_script: None,
            allow_self_payment: true,
            dry_run,
            hop_hints: None,
        })
        .await
    }

    pub async fn assert_payment_status(
        &self,
        payment_hash: Hash256,
        expected_status: PaymentSessionStatus,
        expected_retried: Option<u32>,
    ) {
        let status = self.get_payment_status(payment_hash).await;
        assert_eq!(status, expected_status);

        if let Some(expected_retried) = expected_retried {
            let payment_session = self.get_payment_session(payment_hash).unwrap();
            assert_eq!(payment_session.retried_times, expected_retried);
        }
    }

    pub async fn get_payment_status(&self, payment_hash: Hash256) -> PaymentSessionStatus {
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
        let used_channes = payment_result
            .router
            .nodes
            .iter()
            .map(|r| r.channel_outpoint.clone())
            .collect::<Vec<_>>();
        let funding_tx = self
            .get_channel_funding_tx(&channel_id)
            .expect("funding tx");
        let channel_outpoint = OutPoint::new(funding_tx.into(), 0);
        assert!(used_channes.contains(&channel_outpoint));
    }

    pub async fn wait_until_success(&self, payment_hash: Hash256) {
        loop {
            assert!(self.get_triggered_unexpected_events().await.is_empty());
            let status = self.get_payment_status(payment_hash).await;
            if status == PaymentSessionStatus::Success {
                eprintln!("Payment success: {:?}\n\n", payment_hash);
                break;
            } else if status == PaymentSessionStatus::Failed {
                eprintln!("Payment failed: {:?}\n\n", payment_hash);
                // report error
                assert_eq!(status, PaymentSessionStatus::Success);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn wait_until_failed(&self, payment_hash: Hash256) {
        loop {
            assert!(self.get_triggered_unexpected_events().await.is_empty());
            let status = self.get_payment_status(payment_hash).await;
            if status == PaymentSessionStatus::Failed {
                eprintln!("Payment failed: {:?}\n\n", payment_hash);
                break;
            } else if status == PaymentSessionStatus::Success {
                eprintln!("Payment success: {:?}\n\n", payment_hash);
                // report error
                assert_eq!(status, PaymentSessionStatus::Failed);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn node_info(&self) -> NodeInfoResponse {
        let message =
            |rpc_reply| NetworkActorMessage::Command(NetworkActorCommand::NodeInfo((), rpc_reply));
        eprintln!("query node_info ...");
        let res = call!(self.network_actor, message)
            .expect("node_a alive")
            .unwrap();
        res
    }

    pub async fn update_channel_actor_state(
        &mut self,
        state: ChannelActorState,
        reload_params: Option<ReloadParams>,
    ) {
        let channel_id = state.id.clone();
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
        &mut self,
        channel_id: Hash256,
        new_to_local_amount: u128,
    ) {
        let mut channel_actor_state = self.get_channel_actor_state(channel_id);
        channel_actor_state.to_local_amount = new_to_local_amount;
        self.update_channel_actor_state(channel_actor_state, None)
            .await;
    }

    pub async fn update_channel_remote_balance(
        &mut self,
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

    pub async fn disable_channel_stealthy(&mut self, channel_id: Hash256) {
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

    pub async fn update_channel_with_command(
        &mut self,
        channel_id: Hash256,
        command: UpdateCommand,
    ) {
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

    pub fn get_payment_session(&self, payment_hash: Hash256) -> Option<PaymentSession> {
        self.store.get_payment_session(payment_hash)
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

        let root = get_test_root_actor().await;
        let (event_sender, mut event_receiver) = mpsc::channel(10000);

        let chain_actor = Actor::spawn_linked(None, MockChainActor::new(), (), root.get_cell())
            .await
            .expect("start mock chain actor")
            .0;

        let secret_key: Privkey = fiber_config
            .read_or_generate_secret_key()
            .expect("must generate key")
            .into();
        let public_key = secret_key.pubkey();

        let network_graph = Arc::new(TokioRwLock::new(NetworkGraph::new(
            store.clone(),
            public_key.clone(),
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

        // Some usual unexpected events that we want to not happended
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
        // spwan a new thread to collect all the events from event_receiver
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
            channels_tx_map: Default::default(),
            listening_addrs: announced_addrs,
            network_actor,
            network_graph,
            chain_actor,
            private_key: secret_key.into(),
            peer_id,
            event_emitter: self_event_receiver,
            pubkey: public_key.into(),
            unexpected_events,
            triggered_unexpected_events,
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
        let nodes = Self::new_interconnected_nodes(N).await;
        match nodes.try_into() {
            Ok(nodes) => nodes,
            Err(_) => unreachable!(),
        }
    }

    pub async fn new_interconnected_nodes(n: usize) -> Vec<Self> {
        let mut nodes: Vec<NetworkNode> = Vec::with_capacity(n);
        for i in 0..n {
            let new = Self::new_with_config(
                NetworkNodeConfigBuilder::new()
                    .node_name(Some(format!("node-{}", i)))
                    .base_dir_prefix(&format!("test-fnn-node-{}-", i))
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

    pub async fn new_2_nodes_with_established_channel(
        node_a_funding_amount: u128,
        node_b_funding_amount: u128,
        public: bool,
    ) -> (NetworkNode, NetworkNode, Hash256, TransactionView) {
        let [mut node_a, mut node_b] = NetworkNode::new_n_interconnected_nodes().await;

        let (channel_id, funding_tx) = establish_channel_between_nodes(
            &mut node_a,
            &mut node_b,
            public,
            node_a_funding_amount,
            node_b_funding_amount,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;

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

    pub fn add_channel_tx(&mut self, channel_id: Hash256, tx: TransactionView) {
        self.channels_tx_map.insert(channel_id, tx.hash().into());
    }

    pub fn get_channel_funding_tx(&self, channel_id: &Hash256) -> Option<Hash256> {
        self.channels_tx_map.get(channel_id).cloned()
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

    pub fn get_network_graph(&self) -> &Arc<TokioRwLock<NetworkGraph<Store>>> {
        &self.network_graph
    }

    pub async fn with_network_graph<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&NetworkGraph<Store>) -> T,
    {
        let graph = self.get_network_graph().read().await;
        f(&*graph)
    }

    pub async fn with_network_graph_mut<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut NetworkGraph<Store>) -> T,
    {
        let mut graph = self.get_network_graph().write().await;
        f(&mut graph)
    }

    pub async fn get_network_graph_nodes(&self) -> Vec<NodeInfo> {
        self.with_network_graph(|graph| graph.nodes().into_iter().cloned().collect())
            .await
    }

    pub async fn get_network_graph_node(&self, pubkey: &Pubkey) -> Option<NodeInfo> {
        self.with_network_graph(|graph| graph.get_node(pubkey).cloned())
            .await
    }

    pub async fn get_network_graph_channels(&self) -> Vec<ChannelInfo> {
        self.with_network_graph(|graph| graph.channels().into_iter().cloned().collect())
            .await
    }

    pub async fn get_network_graph_channel(&self, channel_id: &OutPoint) -> Option<ChannelInfo> {
        self.with_network_graph(|graph| {
            tracing::debug!("Getting channel info for {:?}", channel_id);
            tracing::debug!(
                "Channels: {:?}",
                graph.channels().into_iter().collect::<Vec<_>>()
            );
            graph.get_channel(channel_id).cloned()
        })
        .await
    }
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
