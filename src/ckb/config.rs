use ckb_hash::blake2b_256;
use clap_serde_derive::ClapSerde;
use secp256k1::{Secp256k1, SecretKey};
use serde_with::serde_as;
use std::{fs, path::PathBuf, str::FromStr};
use tracing::info;

use crate::utils::encrypt_decrypt_file::{decrypt_from_file, encrypt_to_file};
use crate::{Error, Result};

use ckb_jsonrpc_types::{OutPoint as OutPointWrapper, Script as ScriptWrapper};
use ckb_types::core::ScriptHashType;
use ckb_types::prelude::Builder;
use ckb_types::H256;
use ckb_types::{
    core::DepType,
    packed::{CellDep, Script},
};
use clap_serde_derive::clap::{self};
use molecule::prelude::Entity;
use serde::{Deserialize, Serialize};

use super::contracts::{get_script_by_contract, Contract};

pub const DEFAULT_CKB_BASE_DIR_NAME: &str = "ckb";
const DEFAULT_CKB_NODE_RPC_URL: &str = "http://127.0.0.1:8114";
const ENV_FIBER_SECRET_KEY_PASSWORD: &str = "FIBER_SECRET_KEY_PASSWORD";

#[derive(ClapSerde, Debug, Clone)]
pub struct CkbConfig {
    /// ckb base directory
    #[arg(
        name = "CKB_BASE_DIR",
        long = "ckb-base-dir",
        env,
        help = format!("base directory for ckb actor [default: $BASE_DIR/{}]", DEFAULT_CKB_BASE_DIR_NAME)
    )]
    pub base_dir: Option<PathBuf>,

    #[default(DEFAULT_CKB_NODE_RPC_URL.to_string())]
    #[arg(
        name = "CKB_NODE_RPC_URL",
        long = "ckb-node-rpc-url",
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

    #[default(4000)]
    #[arg(
        name = "CKB_TX_TRACING_POLLING_INTERVAL_MS",
        long = "ckb-tx-tracing-polling-interval-ms",
        env,
        help = "polling interval for ckb tx tracing actor in milliseconds [default: 4000]"
    )]
    pub tx_tracing_polling_interval_ms: u64,
}

impl CkbConfig {
    pub fn base_dir(&self) -> &PathBuf {
        self.base_dir.as_ref().expect("have set base dir")
    }

    pub fn create_base_dir(&self) -> Result<()> {
        if !self.base_dir().exists() {
            std::fs::create_dir_all(self.base_dir()).map_err(Into::into)
        } else {
            Ok(())
        }
    }

    pub fn read_secret_key(&self) -> Result<SecretKey> {
        self.create_base_dir()?;
        let password = std::env::var(ENV_FIBER_SECRET_KEY_PASSWORD).map_err(|_| {
            Error::SecretKeyFileError(format!(
                "please set {} environment variable to encrypt and decrypt the secret key",
                ENV_FIBER_SECRET_KEY_PASSWORD
            ))
        })?;
        let password_bytes = password.as_bytes();

        let path = self.base_dir().join("key");
        if let Ok(plain_key_hex) = fs::read_to_string(&path) {
            if let Ok(plain_key) = hex::decode(plain_key_hex.trim()) {
                info!("secret key is using plain key format, start migrating to encrypted format");
                encrypt_to_file(&path, plain_key.as_ref(), password_bytes)
                    .map_err(Error::SecretKeyFileError)?;
                info!("secret key migration done");
            }
        }

        let key_bin =
            decrypt_from_file(&path, password_bytes).map_err(Error::SecretKeyFileError)?;
        SecretKey::from_slice(&key_bin).map_err(|err| {
            Error::SecretKeyFileError(format!("invalid secret key data, error: {}", err))
        })
    }

    pub fn get_default_funding_lock_script(&self) -> Result<Script> {
        let secret_key = self.read_secret_key()?;
        let secp = Secp256k1::new();
        let pubkey_hash = blake2b_256(secret_key.public_key(&secp).serialize());
        Ok(get_script_by_contract(
            Contract::Secp256k1Lock,
            &pubkey_hash[0..20],
        ))
    }
}

serde_with::serde_conv!(
    ScriptHashTypeWrapper,
    ScriptHashType,
    |s: &ScriptHashType| -> String {
        let v = match s {
            ScriptHashType::Type => "type",
            ScriptHashType::Data => "data",
            ScriptHashType::Data1 => "data1",
            ScriptHashType::Data2 => "data2",
        };
        v.to_string()
    },
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

serde_with::serde_conv!(
    DepTypeWrapper,
    DepType,
    |s: &DepType| -> String {
        let v = match s {
            DepType::Code => "code",
            DepType::DepGroup => "dep_group",
        };
        v.to_string()
    },
    |s: String| {
        let v = match s.to_lowercase().as_str() {
            "code" => DepType::Code,
            "dep_group" => DepType::DepGroup,
            _ => return Err("invalid hash type"),
        };
        Ok(v)
    }
);

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, Default, Eq, PartialEq, Hash)]
pub struct UdtScript {
    pub code_hash: H256,
    #[serde_as(as = "ScriptHashTypeWrapper")]
    pub hash_type: ScriptHashType,
    /// args may be used in pattern matching
    pub args: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq, Hash)]
/// Udt script on-chain dependencies.
pub struct UdtDep {
    /// cell dep described by out_point.
    #[serde(default)]
    pub cell_dep: Option<UdtCellDep>,
    /// cell dep described by type ID.
    #[serde(default)]
    pub type_id: Option<ScriptWrapper>,
}

impl UdtDep {
    pub fn with_cell_dep(cell_dep: UdtCellDep) -> Self {
        Self {
            cell_dep: Some(cell_dep),
            type_id: None,
        }
    }

    pub fn with_type_id(type_id: ScriptWrapper) -> Self {
        Self {
            cell_dep: None,
            type_id: Some(type_id),
        }
    }
}

#[serde_as]
#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct UdtCellDep {
    pub out_point: OutPointWrapper,
    #[serde_as(as = "DepTypeWrapper")]
    pub dep_type: DepType,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct UdtArgInfo {
    pub name: String,
    pub script: UdtScript,
    pub auto_accept_amount: Option<u128>,
    pub cell_deps: Vec<UdtDep>,
}

/// The UDT configurations
#[derive(Serialize, Deserialize, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct UdtCfgInfos(pub Vec<UdtArgInfo>);

impl FromStr for UdtCfgInfos {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl From<&UdtCellDep> for CellDep {
    fn from(cell_dep: &UdtCellDep) -> Self {
        CellDep::new_builder()
            .dep_type(cell_dep.dep_type.into())
            .out_point(cell_dep.out_point.clone().into())
            .build()
    }
}
