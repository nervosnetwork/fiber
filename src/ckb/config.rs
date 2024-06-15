use std::{fs, path::PathBuf, str::FromStr};

use crate::Result;
use ckb_sdk::NetworkType;
use ckb_types::core::ScriptHashType;
use ckb_types::packed::{CellDep, OutPoint};
use ckb_types::prelude::Builder;
use ckb_types::H256;
use ckb_types::{
    packed::{CellDepVec, Script},
    prelude::Pack,
};
use clap::ValueEnum;
use clap_serde_derive::{
    clap::{self},
    ClapSerde,
};
use molecule::prelude::Entity;
use serde::Deserialize;

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
    pub listening_port: u16,

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
    pub announced_node_name: String,

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
    pub open_channel_min_ckb_funding_amount: Option<u128>,
    /// whether to accept open channel requests with ckb funding amount automatically, unit: shannons [default: 6100000000 shannons], if this is set to zero, it means to disable auto accept
    #[arg(
        name = "CKB_AUTO_ACCEPT_CHANNEL_CKB_FUNDING_AMOUNT",
        long = "ckb-auto-accept-channel-ckb-funding-amount",
        env,
        help = "whether to accept open channel requests with ckb funding amount automatically, unit: shannons [default: 6100000000 shannons], if this is set to zero, it means to disable auto accept"
    )]
    pub auto_accept_channel_ckb_funding_amount: Option<u128>,

    #[arg(
        name = "CKB_UDT_WHITELIST",
        long = "ckb-udt-whitelist",
        env,
        value_parser,
        num_args = 0..,
        value_delimiter = ',',
        help = "a list of supported udt scripts"
    )]
    pub udt_whitelist: Option<UdtInfos>,
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

    pub fn open_channel_min_ckb_funding_amount(&self) -> u128 {
        self.open_channel_min_ckb_funding_amount
            .unwrap_or(16100000000)
    }

    pub fn auto_accept_channel_ckb_funding_amount(&self) -> u128 {
        self.auto_accept_channel_ckb_funding_amount
            .unwrap_or(6100000000)
    }

    pub fn udt_whitelist(&self) -> Vec<(String, Script, CellDepVec)> {
        let mut udt_whitelist: Vec<(String, Script, CellDepVec)> = vec![];
        if let Some(udt_infos) = &self.udt_whitelist {
            for udt_info in udt_infos.0.iter() {
                let cell_deps: Vec<CellDep> = udt_info
                    .cell_deps
                    .iter()
                    .map(|dep| CellDep::from(dep))
                    .collect();
                let cell_deps = CellDepVec::new_builder().set(cell_deps).build();
                let script: Script = (&udt_info.script).into();
                udt_whitelist.push((udt_info.name.clone(), script, cell_deps));
            }
        }

        eprintln!("udt_whitelist: {:?}", udt_whitelist);
        return udt_whitelist;
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

#[derive(Debug, Clone, Copy, ValueEnum, Deserialize, PartialEq, Eq)]
pub enum UdtScriptHashType {
    Type,
    Data,
    Data1,
    Data2,
}

#[derive(Deserialize, Debug, Clone)]
pub struct UdtScript {
    pub code_hash: String,
    pub hash_type: UdtScriptHashType,
    pub args: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct UdtCellDep {
    pub dep_type: String,
    pub tx_hash: String,
    pub index: u32,
}

#[derive(Deserialize, Clone, Debug)]
pub struct UdtInfo {
    pub name: String,
    pub script: UdtScript,
    pub cell_deps: Vec<UdtCellDep>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct UdtInfos(Vec<UdtInfo>);

impl FromStr for UdtInfos {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl From<&UdtScript> for Script {
    fn from(script: &UdtScript) -> Self {
        let code_hash = H256::from_str(&script.code_hash).expect("code_hash");
        let _type = match script.hash_type {
            UdtScriptHashType::Data => ScriptHashType::Data,
            UdtScriptHashType::Data1 => ScriptHashType::Data1,
            UdtScriptHashType::Data2 => ScriptHashType::Data2,
            UdtScriptHashType::Type => ScriptHashType::Type,
        };
        let script = Script::new_builder()
            .code_hash(code_hash.pack())
            .hash_type(_type.into())
            .args(script.args.clone().pack())
            .build();
        script
    }
}

impl From<&UdtCellDep> for CellDep {
    fn from(cell_dep: &UdtCellDep) -> Self {
        let dep_type = match cell_dep.dep_type.as_str() {
            "code" => ckb_types::core::DepType::Code,
            "dep_group" => ckb_types::core::DepType::DepGroup,
            _ => panic!("invalid dep type"),
        };
        let tx_hash = H256::from_str(&cell_dep.tx_hash).expect("invalid tx hash");
        CellDep::new_builder()
            .dep_type(dep_type.into())
            .out_point(
                OutPoint::new_builder()
                    .tx_hash(tx_hash.pack())
                    .index(cell_dep.index.pack())
                    .build(),
            )
            .build()
    }
}
