use std::{fs, path::PathBuf};

use ckb_sdk::NetworkType;
use clap::ValueEnum;
use clap_serde_derive::{
    clap::{self},
    ClapSerde,
};
use serde::Deserialize;

use crate::Result;

pub const CKB_SHANNONS: u64 = 100_000_000;
pub const DEFAULT_MIN_INBOUND_LIQUIDITY: u64 = 100 * CKB_SHANNONS; // 100 CKB for minimal inbound liquidity
pub const DEFAULT_MIN_SHUTDOWN_FEE: u64 = 1 * CKB_SHANNONS; // 1 CKB prepared for shutdown transaction fee
pub const DEFAULT_UDT_MINIMAL_CKB_AMOUNT: u64 =
    MIN_UDT_OCCUPIED_CAPACITY + DEFAULT_MIN_SHUTDOWN_FEE; // 143 CKB for minimal UDT amount
pub const MIN_OCCUPIED_CAPACITY: u64 = 61 * CKB_SHANNONS; // 61 CKB for occupied capacity
pub const MIN_UDT_OCCUPIED_CAPACITY: u64 = 142 * CKB_SHANNONS; // 142 CKB for UDT occupied capacity

/// 62 CKB minimal channel amount, at any time a partner should keep at least
/// `MIN_OCCUPIED_CAPACITY` CKB in the channel, so that he can build a valid shutdown transaction
/// and pay proper fee.
pub const MIN_CHANNEL_CAPACITY: u64 = MIN_OCCUPIED_CAPACITY + DEFAULT_MIN_SHUTDOWN_FEE;

/// 162 CKB to open a channel,
/// 100 CKB for minimal inbound liquidity, 61 CKB for occupied capacity
pub const MIN_CHANNEL_OPEN_CAPACITY: u64 =
    DEFAULT_MIN_INBOUND_LIQUIDITY + MIN_OCCUPIED_CAPACITY + DEFAULT_MIN_SHUTDOWN_FEE;

// See comment in `LdkConfig` for why do we need to specify both name and long,
// and prefix them with `ckb-`/`CKB_`.
#[derive(ClapSerde, Debug, Clone)]
pub struct CkbConfig {
    /// ckb base directory
    #[arg(
        name = "CKB_BASE_DIR",
        long = "ckb-base-dir",
        env,
        help = "base directory for ckb [default: $BASE_DIR/ckb]"
    )]
    pub(crate) base_dir: Option<PathBuf>,

    /// listening port for ckb fiber network
    #[arg(name = "CKB_LISTENING_PORT", long = "ckb-listening-port", env)]
    pub(crate) listening_port: u16,

    /// addresses to be announced to ckb fiber network (separated by `,`)
    #[arg(name = "CKB_ANNOUNCED_LISTEN_ADDRS", long = "ckb-announced-listen-addrs", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub(crate) announced_listen_addrs: Vec<String>,

    /// bootstrap node addresses to be connected at startup (separated by `,`)
    #[arg(name = "CKB_BOOTNODES_ADDRS", long = "ckb-bootnodes-addrs", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub bootnode_addrs: Vec<String>,

    /// node name to be announced to lightning network
    #[arg(
        name = "CKB_ANNOUNCED_NODE_NAME",
        long = "ckb-announced-node-name",
        env
    )]
    pub(crate) announced_node_name: String,

    /// name of the network to use (can be any of `mocknet`/`mainnet`/`testnet`/`staging`/`dev`)
    #[arg(name = "CKB_NETWORK", long = "ckb-network", env)]
    pub network: Option<CkbNetwork>,

    /// minimum ckb funding amount for open channel requests, unit: shannons [default: 16100000000 shannons]
    #[arg(
        name = "CKB_OPEN_CHANNEL_MIN_CKB_FUNDING_AMOUNT",
        long = "ckb-open-channel-min-ckb-funding-amount",
        env,
        help = "minimum ckb funding amount for open channel requests, unit: shannons [default: 16100000000 shannons]"
    )]
    pub open_channel_min_ckb_funding_amount: Option<u64>,
    /// whether to accept open channel requests with ckb funding amount automatically, unit: shannons [default: 6100000000 shannons], if this is set to zero, it means to disable auto accept
    #[arg(
        name = "CKB_AUTO_ACCEPT_CHANNEL_CKB_FUNDING_AMOUNT",
        long = "ckb-auto-accept-channel-ckb-funding-amount",
        env,
        help = "whether to accept open channel requests with ckb funding amount automatically, unit: shannons [default: 6100000000 shannons], if this is set to zero, it means to disable auto accept"
    )]
    pub auto_accept_channel_ckb_funding_amount: Option<u64>,
}

impl CkbConfig {
    pub fn open_channel_min_ckb_funding_amount(&self) -> u64 {
        self.open_channel_min_ckb_funding_amount
            .unwrap_or(MIN_CHANNEL_OPEN_CAPACITY)
    }

    pub fn auto_accept_channel_ckb_funding_amount(&self) -> u64 {
        self.auto_accept_channel_ckb_funding_amount
            .unwrap_or(MIN_CHANNEL_CAPACITY)
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

impl CkbConfig {
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
}
