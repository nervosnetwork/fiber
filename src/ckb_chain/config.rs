use ckb_types::prelude::PackVec;
use clap_serde_derive::ClapSerde;
use secp256k1::SecretKey;
use serde_with::serde_as;
use std::{
    io::{ErrorKind, Read},
    path::PathBuf,
};

use std::str::FromStr;

use ckb_types::prelude::Builder;
use ckb_types::prelude::Pack;
use ckb_types::H256;
use ckb_types::{
    core::DepType,
    packed::{CellDep, OutPoint},
};
use ckb_types::{core::ScriptHashType, packed::CellDepVec};
use clap_serde_derive::clap::{self};
use molecule::prelude::Entity;
use serde::Deserialize;

pub const DEFAULT_CKB_CHAIN_BASE_DIR_NAME: &str = "ckb-chain";
const DEFAULT_CKB_CHAIN_NODE_RPC_URL: &str = "http://127.0.0.1:8114";

#[derive(ClapSerde, Debug, Clone)]
pub struct CkbChainConfig {
    /// ckb base directory
    #[arg(
        name = "CKB_CHAIN_BASE_DIR",
        long = "ckb-chain-base-dir",
        env,
        help = format!("base directory for ckb chain actor [default: $BASE_DIR/{}]", DEFAULT_CKB_CHAIN_BASE_DIR_NAME)
    )]
    pub base_dir: Option<PathBuf>,

    #[default(DEFAULT_CKB_CHAIN_NODE_RPC_URL.to_string())]
    #[arg(
        name = "CKB_CHAIN_NODE_RPC_URL",
        long = "ckb-chain-node-rpc-url",
        env,
        help = "rpc url to connect the ckb node [default: http://127.0.0.1:8114]"
    )]
    pub rpc_url: String,

    #[arg(
        name = "CKB_UDT_WHITELIST",
        long = "ckb-udt-whitelist",
        env,
        help = "a list of supported UDT scripts"
    )]
    pub udt_whitelist: Option<UdtCfgInfos>,
}

impl CkbChainConfig {
    pub fn base_dir(&self) -> &PathBuf {
        self.base_dir.as_ref().expect("have set base dir")
    }

    pub fn create_base_dir(&self) -> crate::Result<()> {
        if !self.base_dir().exists() {
            std::fs::create_dir_all(self.base_dir()).map_err(Into::into)
        } else {
            Ok(())
        }
    }

    // TODO: Use keystore and password to read secret key and add an RPC method to authorize the secret key access.
    pub fn read_secret_key(&self) -> crate::Result<SecretKey> {
        self.create_base_dir()?;
        let path = self.base_dir().join("key");
        let mut file = std::fs::File::open(&path)?;

        let warn = |m: bool, d: &str| {
            if m {
                tracing::warn!(
                    "Your secret file's permission is not {}, path: {:?}. \
                Please fix it as soon as possible",
                    d,
                    path
                )
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            warn(
                file.metadata()?.permissions().mode() & 0o177 != 0,
                "less than 0o600",
            );
        }
        #[cfg(not(unix))]
        {
            warn(!file.metadata()?.permissions().readonly(), "readonly");
        }

        let mut key_hex: String = Default::default();
        file.read_to_string(&mut key_hex)?;
        let key_bin = hex::decode(key_hex.trim())
            .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "invalid secret key data"))?;
        SecretKey::from_slice(&key_bin).map_err(|_| {
            std::io::Error::new(ErrorKind::InvalidData, "invalid secret key data").into()
        })
    }
}

serde_with::serde_conv!(
    ScriptHashTypeWrapper,
    ScriptHashType,
    |_: &ScriptHashType| { panic!("no support to serialize") },
    |s: String| {
        let v = match s.to_lowercase().as_str() {
            "type" => ScriptHashType::Type,
            "data" => ScriptHashType::Data,
            "data1" => ScriptHashType::Data1,
            "data2" => ScriptHashType::Data2,
            _ => return Err("invalid hash type"),
        };
        Ok(v)
    }
);

#[serde_as]
#[derive(Deserialize, Debug, Clone, Default)]
pub struct UdtScript {
    pub code_hash: H256,
    #[serde_as(as = "ScriptHashTypeWrapper")]
    pub hash_type: ScriptHashType,
    /// args may be used in pattern matching
    pub args: String,
}

#[derive(Deserialize, Clone, Debug)]
struct UdtCellDep {
    dep_type: String,
    tx_hash: H256,
    index: u32,
}

serde_with::serde_conv!(
    CellDepVecWrapper,
    CellDepVec,
    |_: &CellDepVec| { panic!("no support to serialize") },
    |s: Vec<UdtCellDep>| -> Result<CellDepVec, &'static str> {
        let cell_deps: Vec<CellDep> = s.iter().map(CellDep::from).collect();
        Ok(cell_deps.pack())
    }
);

#[serde_as]
#[derive(Deserialize, Clone, Debug, Default)]
pub struct UdtArgInfo {
    pub name: String,
    pub script: UdtScript,
    pub auto_accept_amount: Option<u128>,
    #[serde_as(as = "CellDepVecWrapper")]
    pub cell_deps: CellDepVec,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct UdtCfgInfos(pub Vec<UdtArgInfo>);

impl FromStr for UdtCfgInfos {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl From<&UdtCellDep> for CellDep {
    fn from(cell_dep: &UdtCellDep) -> Self {
        let dep_type = match cell_dep.dep_type.as_str() {
            "code" => DepType::Code,
            "dep_group" => DepType::DepGroup,
            _ => panic!("invalid dep type"),
        };
        CellDep::new_builder()
            .dep_type(dep_type.into())
            .out_point(
                OutPoint::new_builder()
                    .tx_hash(cell_dep.tx_hash.pack())
                    .index(cell_dep.index.pack())
                    .build(),
            )
            .build()
    }
}
