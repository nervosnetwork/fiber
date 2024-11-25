use crate::{ckb::contracts::Contract, Result};
use ckb_jsonrpc_types::{CellDep, Script};
use clap_serde_derive::{
    clap::{self},
    ClapSerde,
};
#[cfg(not(test))]
use once_cell::sync::OnceCell;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fs, path::PathBuf, str::FromStr};
use tentacle::secio::{PublicKey, SecioKeyPair};

pub const CKB_SHANNONS: u64 = 100_000_000; // 1 CKB = 10 ^ 8 shannons
pub const DEFAULT_MIN_SHUTDOWN_FEE: u64 = 1 * CKB_SHANNONS; // 1 CKB prepared for shutdown transaction fee

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

/// The minimal expiry delta to forward a tlc, in milliseconds. 15 minutes.
pub const MIN_TLC_EXPIRY_DELTA: u64 = 15 * 60 * 1000; // 15 minutes

/// The maximum expiry delta for a payment, in milliseconds. 2 days
pub const MAX_PAYMENT_TLC_EXPIRY_LIMIT: u64 = 2 * 24 * 60 * 60 * 1000; // 2 days

/// The minimal value of a tlc. 0 means no minimal value.
pub const DEFAULT_TLC_MIN_VALUE: u128 = 0;

/// The maximal value of a tlc. 0 means no maximal value.
pub const DEFAULT_TLC_MAX_VALUE: u128 = 0;

/// The fee for forwarding peer tlcs. Proportional to the amount of the forwarded tlc. The unit is millionths of the amount. 1000 means 0.1%.
pub const DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS: u128 = 1000;

/// Whether to automatically announce the node on startup. false means not announcing.
pub const DEFAULT_AUTO_ANNOUNCE_NODE: bool = true;

/// The interval to reannounce NodeAnnouncement, in seconds.
pub const DEFAULT_ANNOUNCE_NODE_INTERVAL_SECONDS: u64 = 3600;

/// Whether to sync the network graph from the network. true means syncing.
pub const DEFAULT_SYNC_NETWORK_GRAPH: bool = true;

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

    /// whether to announce private address, this should be set to false unless you are running a private network or testing [default: false]
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
    #[arg(name = "FIBER_BOOTNODES_ADDRS", long = "fiber-bootnodes-addrs", env, value_parser, num_args = 0.., value_delimiter = ',')]
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
    /// The maximal value of a tlc. [default: 0 (no maximal value)]
    #[arg(
        name = "FIBER_TLC_MAX_VALUE",
        long = "fiber-tlc-max-value",
        env,
        help = "The maximal value of a tlc. [default: 0 (no maximal value)]"
    )]
    pub tlc_max_value: Option<u128>,

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

    /// Whether to sync the network graph from the network. [default: true]
    #[arg(
        name = "FIBER_SYNC_NETWORK_GRAPH",
        long = "fiber-sync-network-graph",
        env,
        help = "Whether to sync the network graph from the network. [default: true]"
    )]
    pub(crate) sync_network_graph: Option<bool>,
}

#[derive(PartialEq, Copy, Clone, Default)]
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

    pub fn from_str(value: &str) -> std::result::Result<Self, String> {
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
        Self::from_str(value).expect("Valid announced node name")
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
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(not(test))]
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

    fn inner_read_or_generate_secret_key(&self) -> Result<super::KeyPair> {
        self.create_base_dir()?;
        super::key::KeyPair::read_or_generate(&self.base_dir().join("sk")).map_err(Into::into)
    }

    // `OnceCell` will make all actors in UI tests use the same secret key.
    // which is not what we want. So we disable it in tests.
    #[cfg(test)]
    pub fn read_or_generate_secret_key(&self) -> Result<super::KeyPair> {
        self.inner_read_or_generate_secret_key()
    }

    #[cfg(not(test))]
    pub fn read_or_generate_secret_key(&self) -> Result<super::KeyPair> {
        FIBER_SECRET_KEY
            .get_or_try_init(|| self.inner_read_or_generate_secret_key())
            .map(|key| key.clone())
    }

    pub fn store_path(&self) -> PathBuf {
        let path = self.base_dir().join("store");
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

    pub fn tlc_max_value(&self) -> u128 {
        self.tlc_max_value.unwrap_or(DEFAULT_TLC_MAX_VALUE)
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

    pub fn sync_network_graph(&self) -> bool {
        self.sync_network_graph
            .unwrap_or(DEFAULT_SYNC_NETWORK_GRAPH)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FiberScript {
    pub name: Contract,
    pub script: Script,
    pub cell_deps: Vec<CellDep>,
}

impl FromStr for FiberScript {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}
