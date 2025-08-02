#[cfg(target_arch = "wasm32")]
use crate::fiber::KeyPair;
use crate::{ckb::contracts::Contract, Result};
use ckb_jsonrpc_types::{CellDep, Script};
use clap_serde_derive::{
    clap::{self},
    ClapSerde,
};
#[cfg(not(any(test, feature = "bench")))]
use once_cell::sync::OnceCell;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fs, path::PathBuf, str::FromStr};
use tentacle::secio::{PublicKey, SecioKeyPair};

use super::features::FeatureVector;

pub const CKB_SHANNONS: u64 = 100_000_000; // 1 CKB = 10 ^ 8 shannons
pub const DEFAULT_MIN_SHUTDOWN_FEE: u64 = CKB_SHANNONS; // 1 CKB prepared for shutdown transaction fee

/// By default, listen to any tcp port allocated by the kernel.
pub const DEFAULT_LISTENING_ADDR: &str = "/ip4/0.0.0.0/tcp/0";

const MIN_OCCUPIED_CAPACITY: u64 = 61 * CKB_SHANNONS; // 61 CKB for occupied capacity

/// Default ckb funding amount when auto accepting an open channel request.
pub const DEFAULT_AUTO_ACCEPT_CHANNEL_CKB_FUNDING_AMOUNT: u64 =
    MIN_OCCUPIED_CAPACITY + DEFAULT_MIN_SHUTDOWN_FEE;

/// Default minimum ckb funding amount for auto accepting an open channel request.
pub const DEFAULT_OPEN_CHANNEL_AUTO_ACCEPT_MIN_CKB_FUNDING_AMOUNT: u64 = 100 * CKB_SHANNONS;

/// The expiry delta to forward a tlc, in milliseconds, default to 1 day.
pub const DEFAULT_TLC_EXPIRY_DELTA: u64 = 24 * 60 * 60 * 1000;

/// 4 hours for each epoch
pub const MILLI_SECONDS_PER_EPOCH: u64 = 4 * 60 * 60 * 1000;

#[cfg(not(debug_assertions))]
/// The minimal expiry delta to forward a tlc, in milliseconds. 16 hours
/// expect it >= 2/3 commitment_delay_epoch, default DEFAULT_COMMITMENT_DELAY_EPOCHS is 6 epoch
/// so 2/3 * 6 = 4 epoch, 4 * 4 hours = 16 hours
pub const MIN_TLC_EXPIRY_DELTA: u64 = 4 * MILLI_SECONDS_PER_EPOCH;
#[cfg(debug_assertions)]
// 5 seconds for testing environment
pub const MIN_TLC_EXPIRY_DELTA: u64 = 5 * 1000;

/// The maximum expiry delta for a payment, in milliseconds. 2 weeks
pub const MAX_PAYMENT_TLC_EXPIRY_LIMIT: u64 = 14 * 24 * 60 * 60 * 1000; // 2 weeks

/// The minimal value of a tlc. 0 means no minimal value.
pub const DEFAULT_TLC_MIN_VALUE: u128 = 0;

/// The fee for forwarding peer tlcs. Proportional to the amount of the forwarded tlc. The unit is millionths of the amount. 1000 means 0.1%.
pub const DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS: u128 = 1000;

/// Whether to automatically announce the node on startup. false means not announcing.
pub const DEFAULT_AUTO_ANNOUNCE_NODE: bool = true;

/// The interval to reannounce NodeAnnouncement, in seconds.
pub const DEFAULT_ANNOUNCE_NODE_INTERVAL_SECONDS: u64 = 3600;

/// The maximum time to hold a tlc, in milliseconds.
#[cfg(not(debug_assertions))]
pub const DEFAULT_HOLD_TLC_TIMEOUT: u64 = 120 * 1000;
#[cfg(debug_assertions)]
pub const DEFAULT_HOLD_TLC_TIMEOUT: u64 = 20 * 1000;

/// The interval to maintain the gossip network, in milli-seconds.
#[cfg(not(any(test, feature = "bench")))]
pub const DEFAULT_GOSSIP_NETWORK_MAINTENANCE_INTERVAL_MS: u64 = 1000 * 60;
#[cfg(any(test, feature = "bench"))]
// This config is needed for the timely processing of gossip messages.
// Without this, some tests may fail due to the delay in processing gossip messages.
pub const DEFAULT_GOSSIP_NETWORK_MAINTENANCE_INTERVAL_MS: u64 = 50;

/// Maximal number of inbound connections.
pub const DEFAULT_MAX_INBOUND_PEERS: usize = 16;

/// Minimal number of outbound connections.
pub const DEFAULT_MIN_OUTBOUND_PEERS: usize = 8;

/// Funding timeout in seconds since the channel is created.
pub const DEFAULT_FUNDING_TIMEOUT_SECONDS: u64 = 60 * 60 * 24; // 1 day

/// The interval to maintain the gossip network, in milli-seconds.
#[cfg(not(any(test, feature = "bench")))]
pub const DEFAULT_GOSSIP_STORE_MAINTENANCE_INTERVAL_MS: u64 = 20 * 1000;
#[cfg(any(test, feature = "bench"))]
// This config is needed for the timely processing of gossip messages.
// Without this, some tests may fail due to the delay in processing gossip messages.
pub const DEFAULT_GOSSIP_STORE_MAINTENANCE_INTERVAL_MS: u64 = 50;

/// Whether to sync the network graph from the network. true means syncing.
pub const DEFAULT_SYNC_NETWORK_GRAPH: bool = true;

/// The maximum number of parts for a multi-part payment.
pub const DEFAULT_MAX_PARTS: u64 = 16;
pub const PAYMENT_MAX_PARTS_LIMIT: u64 = 64;

// See comment in `LdkConfig` for why do we need to specify both name and long,
// and prefix them with `ckb-`/`CKB_`.
#[derive(ClapSerde, Debug, Clone)]
pub struct FiberConfig {
    /// ckb base directory
    #[arg(
        name = "FIBER_BASE_DIR",
        long = "fiber-base-dir",
        env,
        help = "base directory for fiber [default: $BASE_DIR/fiber]"
    )]
    pub(crate) base_dir: Option<PathBuf>,

    /// listening address for fiber network [default: "/ip4/0.0.0.0/tcp/0" (random tcp port)]
    #[arg(name = "FIBER_LISTENING_ADDR", long = "fiber-listening-addr", env)]
    pub(crate) listening_addr: Option<String>,

    /// whether to announce listening address [default: false]
    #[arg(
        name = "FIBER_ANNOUNCE_LISTENING_ADDR",
        long = "fiber-announce-listening-addr",
        env
    )]
    pub(crate) announce_listening_addr: Option<bool>,

    /// whether to announce or process private address, this should be set to false unless you are running a private network or testing [default: false]
    #[arg(
        name = "FIBER_ANNOUNCE_PRIVATE_ADDR",
        long = "fiber-announce-private-addr",
        env
    )]
    pub(crate) announce_private_addr: Option<bool>,

    /// addresses to be announced to fiber network (separated by `,`)
    #[arg(name = "FIBER_ANNOUNCED_ADDRS", long = "fiber-announced-addrs", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub(crate) announced_addrs: Vec<String>,

    /// bootstrap node addresses to be connected at startup (separated by `,`)
    #[arg(name = "FIBER_BOOTNODE_ADDRS", long = "fiber-bootnode-addrs", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub bootnode_addrs: Vec<String>,

    /// node name to be announced to fiber network
    #[arg(
        name = "FIBER_ANNOUNCED_NODE_NAME",
        long = "fiber-announced-node-name",
        env
    )]
    pub(crate) announced_node_name: Option<AnnouncedNodeName>,

    /// chain spec file path, can be "mainnet", "testnet", or a file path to a custom chain spec
    #[arg(name = "FIBER_CHAIN", long = "fiber-chain", env)]
    pub chain: String,

    /// lock script configurations related to fiber network
    #[arg(name = "FIBER_SCRIPTS", long = "fiber-scripts", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub scripts: Vec<FiberScript>,

    /// minimum ckb funding amount for auto accepting an open channel requests, unit: shannons [default: 10000000000 shannons]
    #[arg(
        name = "FIBER_OPEN_CHANNEL_AUTO_ACCEPT_MIN_CKB_FUNDING_AMOUNT",
        long = "fiber-open-channel-auto-accept-min-ckb-funding-amount",
        env,
        help = "minimum ckb funding amount for auto accepting an open channel requests, unit: shannons [default: 10000000000 shannons]"
    )]
    pub open_channel_auto_accept_min_ckb_funding_amount: Option<u64>,
    /// whether to accept open channel requests with ckb funding amount automatically, unit: shannons [default: 6200000000 shannons], if this is set to zero, it means to disable auto accept
    #[arg(
        name = "FIBER_AUTO_ACCEPT_CHANNEL_CKB_FUNDING_AMOUNT",
        long = "fiber-auto-accept-channel-ckb-funding-amount",
        env,
        help = "whether to accept open channel requests with ckb funding amount automatically, unit: shannons [default: 6200000000 shannons], if this is set to zero, it means to disable auto accept"
    )]
    pub auto_accept_channel_ckb_funding_amount: Option<u64>,

    /// The expiry delta to forward a tlc, in milliseconds. [default: 86400000 (1 day)]
    #[arg(
        name = "FIBER_TLC_EXPIRY_DELTA",
        long = "fiber-tlc-expiry-delta",
        env,
        help = "The expiry delta to forward a tlc, in milliseconds. [default: 86400000 (1 day)]"
    )]
    pub tlc_expiry_delta: Option<u64>,

    /// The minimal value of a tlc. [default: 0 (no minimal value)]
    #[arg(
        name = "FIBER_TLC_MIN_VALUE",
        long = "fiber-tlc-min-value",
        env,
        help = "The minimal value of a tlc. [default: 0 (no minimal value)]"
    )]
    pub tlc_min_value: Option<u128>,

    /// The fee for forwarding peer tlcs. Proportional to the amount of the forwarded tlc. The unit is millionths of the amount. [default: 1000 (0.1%)]
    #[arg(
        name = "FIBER_TLC_FEE_PROPORTIONAL_MILLIONTHS",
        long = "fiber-tlc-fee-proportional-millionths",
        env,
        help = "The fee for forwarding peer tlcs. Proportional to the amount of the forwarded tlc. The unit is millionths of the amount. [default: 1000 (0.1%)]"
    )]
    pub tlc_fee_proportional_millionths: Option<u128>,

    /// Whether to automatically announce the node on startup. [default: true]
    #[arg(
        name = "FIBER_AUTO_ANNOUNCE_NODE",
        long = "fiber-auto-announce-node",
        env,
        help = "Whether to automatically announce the node on startup. [default: true]"
    )]
    pub auto_announce_node: Option<bool>,

    // TODO: the more sensible default value for this option is a reasonable interval like one day
    // if this node has public channels, otherwise don't reannounce (or announce) at all.
    /// The interval to reannounce NodeAnnouncement, in seconds. 0 means never reannounce. [default: 3600 (1 hour)]
    #[arg(
        name = "FIBER_ANNOUNCE_NODE_INTERVAL_SECONDS",
        long = "fiber-announce-node-interval-seconds",
        env,
        help = "The interval to reannounce NodeAnnouncement, in seconds. 0 means never reannounce. [default: 3600 (1 hour)]"
    )]
    pub(crate) announce_node_interval_seconds: Option<u64>,

    /// Gossip network maintenance interval, in milli-seconds. [default: 60000]
    /// This is the interval to maintain the gossip network, including connecting to more peers, etc.
    #[arg(
        name = "FIBER_GOSSIP_NETWORK_MAINTENANCE_INTERVAL_MS",
        long = "fiber-gossip-network-maintenance-interval-ms",
        env,
        help = "Gossip network maintenance interval, in milli-seconds. [default: 60000]"
    )]
    pub(crate) gossip_network_maintenance_interval_ms: Option<u64>,

    /// Maximal number of inbound connections. The node will disconnect inbound connections
    /// when the number of inbound connection exceeds this number. [default: 16]
    #[arg(
        name = "FIBER_MAX_INBOUND_PEERS",
        long = "fiber-max-inbound-peers",
        env,
        help = "Maximal number of inbound connections. The node will disconnect inbound connections when the number of inbound connection exceeds this number. [default: 16]"
    )]
    pub(crate) max_inbound_peers: Option<usize>,

    /// Minimal number of outbound connections. The node will try to connect to more peers
    /// when the number of outbound connection is less than this number. [default: 8]
    #[arg(
        name = "FIBER_MIN_OUTBOUND_PEERS",
        long = "fiber-min-outbound-peers",
        env,
        help = "Minimal number of outbound connections. The node will try to connect to more peers when the number of outbound connection is less than this number. [default: 8]"
    )]
    pub(crate) min_outbound_peers: Option<usize>,

    /// Gossip store maintenance interval, in milli-seconds. [default: 20000]
    /// This is the interval to maintain the gossip store, including saving messages whose complete dependencies
    /// are available, etc.
    #[arg(
        name = "FIBER_GOSSIP_STORE_MAINTENANCE_INTERVAL_MS",
        long = "fiber-gossip-store-maintenance-interval-ms",
        env,
        help = "Gossip store maintenance interval, in milli-seconds. [default: 20000]"
    )]
    pub(crate) gossip_store_maintenance_interval_ms: Option<u64>,

    /// Gossip network num targeted active syncing peers. [default: None]
    /// This is the number of peers to target for active syncing. This is the number of peers that we will
    /// send GetBroadcastMessages message to obtain the gossip messages that we missed during the time we
    /// were offiline. A larger number means more peers to receive updates from, but also more bandwidth usage.
    /// If None, it will use the default value.
    #[arg(
        name = "FIBER_GOSSIP_NETWORK_NUM_TARGETED_ACTIVE_SYNCING_PEERS",
        long = "fiber-gossip-network-num-targeted-active-syncing-peers",
        env,
        help = "Gossip network num targeted active syncing peers. [default: None]"
    )]
    pub(crate) gossip_network_num_targeted_active_syncing_peers: Option<usize>,

    /// Gossip network num targeted outbound passive syncing peers. [default: None]
    /// This is the number of peers to target for outbound passive syncing. This is the number of outbound peers
    /// that we will send BroadcastMessageFilter to receive updates from them. A larger number means more
    /// peers to receive updates from, but also more bandwidth usage. We only count the outbound peers here,
    /// because outbound peers are less likely to be malicious, and we want to receive updates from them.
    /// If None, it will use the default value.
    #[arg(
        name = "FIBER_GOSSIP_NETWORK_NUM_TARGETED_OUTBOUND_PASSIVE_SYNCING_PEERS",
        long = "fiber-gossip-network-num-targeted-outbound-passive-syncing-peers",
        env,
        help = "Gossip network num targeted outbound passive syncing peers. [default: None]"
    )]
    pub(crate) gossip_network_num_targeted_outbound_passive_syncing_peers: Option<usize>,

    /// Whether to sync the network graph from the network. [default: true]
    #[arg(
        name = "FIBER_SYNC_NETWORK_GRAPH",
        long = "fiber-sync-network-graph",
        env,
        help = "Whether to sync the network graph from the network. [default: true]"
    )]
    pub(crate) sync_network_graph: Option<bool>,

    /// The interval to check watchtower, in seconds. 0 means never check. [default: 60 (1 minute)]
    #[arg(
        name = "FIBER_WATCHTOWER_CHECK_INTERVAL_SECONDS",
        long = "fiber-watchtower-check-interval-seconds",
        env,
        help = "The interval to check watchtower, in seconds. 0 means never check. [default: 60 (1 minute)]"
    )]
    pub watchtower_check_interval_seconds: Option<u64>,

    /// The url of the standalone watchtower rpc server. [default: None]
    #[arg(
        name = "FIBER_STANDALONE_WATCHTOWER_RPC_URL",
        long = "fiber-standalone-watchtower-rpc-url",
        env,
        help = "The url of the standalone watchtower rpc server. [default: None]"
    )]
    pub standalone_watchtower_rpc_url: Option<String>,

    /// The RPC token of the standalone watchtower. [default: None]
    #[arg(
        name = "FIBER_STANDALONE_WATCHTOWER_TOKEN",
        long = "fiber-standalone-watchtower-token",
        env,
        help = "The RPC token of the standalone watchtower. [default: None]"
    )]
    pub standalone_watchtower_token: Option<String>,

    /// Disable built-in watchtower actor. [default: false]
    #[arg(
        name = "FIBER_DISABLE_BUILT_IN_WATCHTOWER",
        long = "fiber-disable-built-in-watchtower",
        env,
        help = "Disable built-in watchtower actor. [default: false]"
    )]
    pub disable_built_in_watchtower: Option<bool>,
    #[cfg(target_arch = "wasm32")]
    #[arg(skip)]
    pub wasm_key_pair: Option<KeyPair>,

    /// Max allowed number of channels to be accepted from one peer. [default: 20]
    #[arg(
        name = "FIBER_TO_BE_ACCEPTED_CHANNELS_NUMBER_LIMIT",
        long = "fiber-to-be-accepted-channels-number-limit",
        env,
        help = "Max allowed number of channels to be accepted from one peer. [default: 20]"
    )]
    pub to_be_accepted_channels_number_limit: Option<usize>,

    /// Max allowed storage bytes of channels to be accepted from one peer. [default: 50KB]
    #[arg(
        name = "FIBER_TO_BE_ACCEPTED_CHANNELS_BYTESS_LIMIT",
        long = "fiber-to-be-accepted-channels-bytes-limit",
        env,
        help = "Max allowed bytes of channels to be accepted from one peer. [default: 50KB]"
    )]
    pub to_be_accepted_channels_bytes_limit: Option<usize>,

    /// Default timeout to auto close a funding channel. [default: 1 day]
    #[arg(
        name = "FIBER_FUNDING_TIMEOUT_SECONDS",
        long = "fiber-funding-timeout-seconds",
        env,
        help = "Default timeout to auto close a funding channel. [default: 1 day]"
    )]
    #[default(DEFAULT_FUNDING_TIMEOUT_SECONDS)]
    pub funding_timeout_seconds: u64,

    /// Use an external shell command to build funding tx.
    ///
    /// The command is executed by `cmd /C` in Windows, and by `sh -c` in other systems.
    ///
    /// The command receives a JSON object from stdin with following keys:
    /// - `tx`: The current `Transaction`. This can be `null` for the first funding request.
    /// - `request`: The `FundingRequest` to fulfil.
    ///
    /// The command MUST use non-zero exit status to indicate failures and print error message to stderr.
    /// It MUST print Transaction in JSON to stdout on success building.
    #[arg(
        name = "FIBER_FUNDING_TX_SHELL_BUILDER",
        long = "fiber-funding-tx-shell-builder",
        env,
        help = "Use an external shell command to build funding tx. [default: None]"
    )]
    pub funding_tx_shell_builder: Option<String>,

    /// Listen to WebSocket on the same TCP port
    #[arg(
        name = "FIBER_REUSE_PORT_FOR_WEBSOCKET",
        long = "fiber-reuse-port-for-websocket",
        env,
        help = "Whether to re-use the same TCP port to listen for WebSocket [default: true]"
    )]
    #[default(true)]
    pub reuse_port_for_websocket: bool,
}

/// Must be a valid utf-8 string of length maximal length 32 bytes.
/// If the length is less than 32 bytes, it will be padded with 0.
/// If the length is more than 32 bytes, it should be truncated.
#[derive(Eq, PartialEq, Copy, Clone, Default, Hash)]
pub struct AnnouncedNodeName(pub [u8; 32]);

impl AnnouncedNodeName {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_slice(slice: &[u8]) -> std::result::Result<Self, String> {
        if slice.len() > 32 {
            return Err("Node Alias can not be longer than 32 bytes".to_string());
        }
        let mut bytes = [0; 32];
        bytes[..slice.len()].copy_from_slice(slice);
        Ok(Self(bytes))
    }

    pub fn from_string(value: &str) -> std::result::Result<Self, String> {
        let str_bytes = value.as_bytes();
        Self::from_slice(str_bytes)
    }

    pub fn as_str(&self) -> &str {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(self.0.len());
        if end == 0 {
            return "";
        }
        std::str::from_utf8(&self.0[..end]).expect("valid utf8 string")
    }
}

impl std::fmt::Display for AnnouncedNodeName {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::fmt::Debug for AnnouncedNodeName {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "AnnouncedNodeName({})", self)
    }
}

impl<'s> From<&'s str> for AnnouncedNodeName {
    fn from(value: &'s str) -> Self {
        Self::from_string(value).expect("Valid announced node name")
    }
}

impl serde::Serialize for AnnouncedNodeName {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(std::str::from_utf8(&self.0).expect("valid utf8 string"))
    }
}

impl<'de> serde::Deserialize<'de> for AnnouncedNodeName {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_string(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(not(any(test, feature = "bench")))]
static FIBER_SECRET_KEY: OnceCell<super::KeyPair> = OnceCell::new();

impl FiberConfig {
    pub fn base_dir(&self) -> &PathBuf {
        self.base_dir.as_ref().expect("have set base dir")
    }

    pub fn create_base_dir(&self) -> Result<()> {
        if !self.base_dir().exists() {
            fs::create_dir_all(self.base_dir()).map_err(Into::into)
        } else {
            Ok(())
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn inner_read_or_generate_secret_key(&self) -> Result<super::KeyPair> {
        self.create_base_dir()?;
        super::key::KeyPair::read_or_generate(&self.base_dir().join("sk")).map_err(Into::into)
    }

    #[cfg(target_arch = "wasm32")]
    fn inner_read_or_generate_secret_key(&self) -> Result<super::KeyPair> {
        return Ok(self
            .wasm_key_pair
            .clone()
            .expect("SecretKey on wasm not found!"));
    }

    // `OnceCell` will make all actors in UI tests use the same secret key.
    // which is not what we want. So we disable it in tests.
    #[cfg(any(test, feature = "bench"))]
    pub fn read_or_generate_secret_key(&self) -> Result<super::KeyPair> {
        self.inner_read_or_generate_secret_key()
    }

    #[cfg(not(any(test, feature = "bench")))]
    pub fn read_or_generate_secret_key(&self) -> Result<super::KeyPair> {
        FIBER_SECRET_KEY
            .get_or_try_init(|| self.inner_read_or_generate_secret_key())
            .cloned()
    }

    pub fn store_path(&self) -> PathBuf {
        let path = self.base_dir().join("store");
        #[cfg(not(target_arch = "wasm32"))]
        if !path.exists() {
            fs::create_dir_all(&path).expect("create store directory");
        }
        path
    }

    pub fn listening_addr(&self) -> &str {
        self.listening_addr
            .as_deref()
            .unwrap_or(DEFAULT_LISTENING_ADDR)
    }

    pub fn announce_listening_addr(&self) -> bool {
        self.announce_listening_addr.unwrap_or(false)
    }

    pub fn announce_private_addr(&self) -> bool {
        self.announce_private_addr.unwrap_or(false)
    }

    pub fn open_channel_auto_accept_min_ckb_funding_amount(&self) -> u64 {
        self.open_channel_auto_accept_min_ckb_funding_amount
            .unwrap_or(DEFAULT_OPEN_CHANNEL_AUTO_ACCEPT_MIN_CKB_FUNDING_AMOUNT)
    }

    pub fn auto_accept_channel_ckb_funding_amount(&self) -> u64 {
        self.auto_accept_channel_ckb_funding_amount
            .unwrap_or(DEFAULT_AUTO_ACCEPT_CHANNEL_CKB_FUNDING_AMOUNT)
    }

    pub fn tlc_expiry_delta(&self) -> u64 {
        self.tlc_expiry_delta.unwrap_or(DEFAULT_TLC_EXPIRY_DELTA)
    }

    pub fn tlc_min_value(&self) -> u128 {
        self.tlc_min_value.unwrap_or(DEFAULT_TLC_MIN_VALUE)
    }

    pub fn tlc_fee_proportional_millionths(&self) -> u128 {
        self.tlc_fee_proportional_millionths
            .unwrap_or(DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS)
    }

    pub fn auto_announce_node(&self) -> bool {
        self.auto_announce_node
            .unwrap_or(DEFAULT_AUTO_ANNOUNCE_NODE)
    }

    pub fn announce_node_interval_seconds(&self) -> u64 {
        self.announce_node_interval_seconds
            .unwrap_or(DEFAULT_ANNOUNCE_NODE_INTERVAL_SECONDS)
    }

    pub fn public_key(&self) -> PublicKey {
        let secio_kp: SecioKeyPair = self
            .read_or_generate_secret_key()
            .expect("read or generate secret key")
            .into();
        secio_kp.public_key()
    }

    pub fn gossip_network_maintenance_interval_ms(&self) -> u64 {
        self.gossip_network_maintenance_interval_ms
            .unwrap_or(DEFAULT_GOSSIP_NETWORK_MAINTENANCE_INTERVAL_MS)
    }

    pub fn max_inbound_peers(&self) -> usize {
        self.max_inbound_peers.unwrap_or(DEFAULT_MAX_INBOUND_PEERS)
    }

    pub fn min_outbound_peers(&self) -> usize {
        self.min_outbound_peers
            .unwrap_or(DEFAULT_MIN_OUTBOUND_PEERS)
    }

    pub fn gossip_store_maintenance_interval_ms(&self) -> u64 {
        self.gossip_store_maintenance_interval_ms
            .unwrap_or(DEFAULT_GOSSIP_STORE_MAINTENANCE_INTERVAL_MS)
    }

    pub fn sync_network_graph(&self) -> bool {
        self.sync_network_graph
            .unwrap_or(DEFAULT_SYNC_NETWORK_GRAPH)
    }

    pub fn gen_node_features(&self) -> FeatureVector {
        // TODO: override default features from config settings
        // ...
        FeatureVector::default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ScriptCellDep {
    #[serde(default)]
    pub cell_dep: Option<CellDep>,
    #[serde(default)]
    pub type_id: Option<Script>,
}

impl ScriptCellDep {
    pub fn with_cell_dep(cell_dep: CellDep) -> Self {
        Self {
            cell_dep: Some(cell_dep),
            type_id: None,
        }
    }

    pub fn with_type_id(type_id: Script) -> Self {
        Self {
            cell_dep: None,
            type_id: Some(type_id),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FiberScript {
    pub name: Contract,
    pub script: Script,
    /// Type ID of the cell deps
    pub cell_deps: Vec<ScriptCellDep>,
}

impl FromStr for FiberScript {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}
