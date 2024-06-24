use clap_serde_derive::ClapSerde;

use secp256k1::SecretKey;
use std::{
    io::{ErrorKind, Read},
    path::PathBuf,
};

use std::str::FromStr;

use ckb_types::prelude::Builder;
use ckb_types::H256;
use ckb_types::{
    core::DepType,
    packed::{CellDep, OutPoint},
};
use ckb_types::{core::ScriptHashType, packed::CellDepVec};
use ckb_types::{packed::Script, prelude::Pack};
use clap::ValueEnum;
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
        value_parser,
        help = "a list of supported UDT scripts"
    )]
    udt_whitelist: Option<UdtInfos>,
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
                log::warn!(
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

    pub fn udt_whitelist(&self) -> Vec<UdtScriptInfo> {
        let mut udt_whitelist: Vec<UdtScriptInfo> = vec![];
        if let Some(udt_infos) = &self.udt_whitelist {
            for udt_info in udt_infos.0.iter() {
                let cell_deps: Vec<CellDep> = udt_info
                    .cell_deps
                    .iter()
                    .map(|dep| CellDep::from(dep))
                    .collect();
                let cell_deps = CellDepVec::new_builder().set(cell_deps).build();
                let script: Script = (&udt_info.script).into();
                let arg_pattern = udt_info.script.args.clone();
                udt_whitelist.push((udt_info.name.clone(), script, arg_pattern, cell_deps));
            }
        }
        return udt_whitelist;
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, Deserialize, PartialEq, Eq)]
enum UdtScriptHashType {
    Type,
    Data,
    Data1,
    Data2,
}

#[derive(Deserialize, Debug, Clone)]
struct UdtScript {
    code_hash: H256,
    hash_type: UdtScriptHashType,
    /// args may be used in pattern matching
    args: String,
}

#[derive(Deserialize, Clone, Debug)]
struct UdtCellDep {
    dep_type: String,
    tx_hash: H256,
    index: u32,
}

#[derive(Deserialize, Clone, Debug)]
struct UdtInfo {
    name: String,
    script: UdtScript,
    cell_deps: Vec<UdtCellDep>,
}

#[derive(Deserialize, Clone, Debug)]
struct UdtInfos(Vec<UdtInfo>);

pub type UdtScriptInfo = (String, Script, String, CellDepVec);

impl FromStr for UdtInfos {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl From<&UdtScript> for Script {
    fn from(script: &UdtScript) -> Self {
        let _type = match script.hash_type {
            UdtScriptHashType::Data => ScriptHashType::Data,
            UdtScriptHashType::Data1 => ScriptHashType::Data1,
            UdtScriptHashType::Data2 => ScriptHashType::Data2,
            UdtScriptHashType::Type => ScriptHashType::Type,
        };
        let mut builder = Script::new_builder()
            .code_hash(script.code_hash.pack())
            .hash_type(_type.into());

        let arg = script.args.strip_prefix("0x").unwrap_or(&script.args);
        if let Ok(packed_args) = H256::from_str(arg) {
            builder = builder.args(packed_args.as_bytes().pack());
        }
        builder.build()
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
