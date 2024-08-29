use crate::Result;
use ckb_sdk::NetworkType;
use clap::ValueEnum;
use clap_serde_derive::{
    clap::{self},
    ClapSerde,
};
use serde::{Deserialize, Deserializer, Serializer};
use std::{fs, path::PathBuf};

pub const CKB_SHANNONS: u64 = 100_000_000; // 1 CKB = 10 ^ 8 shannons
pub const DEFAULT_MIN_INBOUND_LIQUIDITY: u64 = 100 * CKB_SHANNONS; // 100 CKB for minimal inbound liquidity
pub const DEFAULT_MIN_SHUTDOWN_FEE: u64 = CKB_SHANNONS; // 1 CKB prepared for shutdown transaction fee
pub const MIN_OCCUPIED_CAPACITY: u64 = 61 * CKB_SHANNONS; // 61 CKB for occupied capacity
pub const MIN_UDT_OCCUPIED_CAPACITY: u64 = 142 * CKB_SHANNONS; // 142 CKB for UDT occupied capacity

/// 62 CKB minimal channel amount, at any time a partner should keep at least
/// `DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT` CKB in the channel,
/// to make sure he can build a valid shutdown transaction and pay proper fee.
pub const DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT: u64 =
    MIN_OCCUPIED_CAPACITY + DEFAULT_MIN_SHUTDOWN_FEE;

/// 143 CKB for minimal UDT amount
pub const DEFAULT_UDT_MINIMAL_CKB_AMOUNT: u64 =
    MIN_UDT_OCCUPIED_CAPACITY + DEFAULT_MIN_SHUTDOWN_FEE;

/// 162 CKB to open a channel which maybe automatically acceptted.
/// 100 CKB for minimal inbound liquidity, 61 CKB for occupied capacity, 1 CKB for shutdown fee
/// The other party may auto accept the channel if the amount is greater than this.
pub const DEFAULT_CHANNEL_MIN_AUTO_CKB_AMOUNT: u64 =
    DEFAULT_MIN_INBOUND_LIQUIDITY + MIN_OCCUPIED_CAPACITY + DEFAULT_MIN_SHUTDOWN_FEE;

/// The locktime expiry delta to forward a tlc, in seconds. 86400 means 1 day.
pub const DEFAULT_TLC_LOCKTIME_EXPIRY_DELTA: u64 = 86400;

/// The minimal value of a tlc. 0 means no minimal value.
pub const DEFAULT_TLC_MIN_VALUE: u128 = 0;

/// The maximal value of a tlc. 0 means no maximal value.
pub const DEFAULT_TLC_MAX_VALUE: u128 = 0;

/// The fee for forwarding peer tlcs. Proportional to the amount of the forwarded tlc. The unit is millionths of the amount. 1000 means 0.1%.
pub const DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS: u32 = 1000;

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

    /// listening port for fiber network
    #[arg(name = "FIBER_LISTENING_PORT", long = "fiber-listening-port", env)]
    pub(crate) listening_port: u16,

    /// addresses to be announced to fiber network (separated by `,`)
    #[arg(name = "FIBER_ANNOUNCED_LISTEN_ADDRS", long = "fiber-announced-listen-addrs", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub(crate) announced_listen_addrs: Vec<String>,

    /// bootstrap node addresses to be connected at startup (separated by `,`)
    #[arg(name = "FIBER_BOOTNODES_ADDRS", long = "fiber-bootnodes-addrs", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub bootnode_addrs: Vec<String>,

    /// node name to be announced to fiber network
    #[arg(
        name = "fiber_ANNOUNCED_NODE_NAME",
        long = "fiber-announced-node-name",
        env
    )]
    pub(crate) announced_node_name: Option<AnnouncedNodeName>,

    /// name of the network to use (can be any of `mocknet`/`mainnet`/`testnet`/`staging`/`dev`)
    #[arg(name = "FIBER_NETWORK", long = "fiber-network", env)]
    pub network: Option<CkbNetwork>,

    /// minimum ckb funding amount for auto accepting an open channel requests, aunit: shannons [default: 16200000000 shannons]
    #[arg(
        name = "FIBER_OPEN_CHANNEL_AUTO_ACCEPT_MIN_CKB_FUNDING_AMOUNT",
        long = "fiber-open-channel-auto-accept-min-ckb-funding-amount",
        env,
        help = "minimum ckb funding amount for auto accepting an open channel requests, unit: shannons [default: 16200000000 shannons]"
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

    /// The locktime expiry delta to forward a tlc, in seconds. [default: 86400 (1 day)]
    #[arg(
        name = "FIBER_TLC_LOCKTIME_EXPIRY_DELTA",
        long = "fiber-tlc-locktime-expiry-delta",
        env,
        help = "The locktime expiry delta to forward a tlc, in seconds. [default: 86400 (1 day)]"
    )]
    pub tlc_locktime_expiry_delta: Option<u64>,

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
    pub tlc_fee_proportional_millionths: Option<u32>,
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
        std::str::from_utf8(&self.0).expect("valid utf8 string")
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

    pub fn read_or_generate_secret_key(&self) -> Result<super::KeyPair> {
        self.create_base_dir()?;
        super::key::KeyPair::read_or_generate(&self.base_dir().join("sk")).map_err(Into::into)
    }

    pub fn store_path(&self) -> PathBuf {
        let path = self.base_dir().join("store");
        if !path.exists() {
            fs::create_dir_all(&path).expect("create store directory");
        }
        path
    }

    pub fn open_channel_auto_accept_min_ckb_funding_amount(&self) -> u64 {
        self.open_channel_auto_accept_min_ckb_funding_amount
            .unwrap_or(DEFAULT_CHANNEL_MIN_AUTO_CKB_AMOUNT)
    }

    pub fn auto_accept_channel_ckb_funding_amount(&self) -> u64 {
        self.auto_accept_channel_ckb_funding_amount
            .unwrap_or(DEFAULT_CHANNEL_MINIMAL_CKB_AMOUNT)
    }

    pub fn tlc_locktime_expiry_delta(&self) -> u64 {
        self.tlc_locktime_expiry_delta
            .unwrap_or(DEFAULT_TLC_LOCKTIME_EXPIRY_DELTA)
    }

    pub fn tlc_min_value(&self) -> u128 {
        self.tlc_min_value.unwrap_or(DEFAULT_TLC_MIN_VALUE)
    }

    pub fn tlc_max_value(&self) -> u128 {
        self.tlc_max_value.unwrap_or(DEFAULT_TLC_MAX_VALUE)
    }

    pub fn tlc_fee_proportional_millionths(&self) -> u32 {
        self.tlc_fee_proportional_millionths
            .unwrap_or(DEFAULT_TLC_FEE_PROPORTIONAL_MILLIONTHS)
    }
}

// Basically ckb_sdk::types::NetworkType. But we added a `Mocknet` variant.
// And we can't use `ckb_sdk::types::NetworkType` directly because it is not `ValueEnum`.
#[derive(Debug, Clone, Copy, ValueEnum, Deserialize, PartialEq, Eq)]
pub enum CkbNetwork {
    Mocknet,
    Mainnet,
    Testnet,
    Staging,
    Dev,
}

impl From<CkbNetwork> for Option<NetworkType> {
    fn from(network: CkbNetwork) -> Self {
        match network {
            CkbNetwork::Mocknet => None,
            CkbNetwork::Mainnet => Some(NetworkType::Mainnet),
            CkbNetwork::Testnet => Some(NetworkType::Testnet),
            CkbNetwork::Staging => Some(NetworkType::Staging),
            CkbNetwork::Dev => Some(NetworkType::Dev),
        }
    }
}
