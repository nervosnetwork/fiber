use super::contracts::{get_script_by_contract, Contract};
use crate::ckb::contracts::ScriptCellDep;
#[cfg(not(target_arch = "wasm32"))]
use crate::utils::encrypt_decrypt_file::{decrypt_from_file, encrypt_to_file};
use crate::Result;
use ckb_jsonrpc_types::{OutPoint as OutPointWrapper, Script as ScriptWrapper};
use ckb_sdk::{traits::DefaultCellCollector, CkbRpcAsyncClient};
use ckb_types::prelude::Builder;
use ckb_types::H256;
use ckb_types::{
    core::DepType,
    packed::{CellDep, Script},
};
use ckb_types::{core::ScriptHashType, prelude::Unpack};
use clap_serde_derive::clap::{self};
use clap_serde_derive::ClapSerde;
use molecule::prelude::Entity;
use secp256k1::SecretKey;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
#[cfg(not(target_arch = "wasm32"))]
use std::fs;
#[cfg(not(target_arch = "wasm32"))]
use tracing::info;
use {ckb_hash::blake2b_256, secp256k1::Secp256k1};

use std::{path::PathBuf, str::FromStr};

pub const DEFAULT_CKB_BASE_DIR_NAME: &str = "ckb";
const DEFAULT_CKB_NODE_RPC_URL: &str = "http://127.0.0.1:8114";
#[cfg(not(target_arch = "wasm32"))]
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

    /// Use an external shell command to build funding tx.
    ///
    /// The command is executed by `cmd /C` in Windows, and by `sh -c` in other systems.
    ///
    /// The command receives a JSON object from stdin with following keys:
    /// - `tx`: The current `Transaction`. This can be `null` for the first funding request.
    /// - `request`: The `FundingRequest` to fulfil.
    /// - `rpc_url`: RPC URL to connect the CKB node.
    /// - `funding_source_lock_script`: The lock script who provides input cells for funding.
    ///
    /// Example:
    ///
    /// ```json
    /// ```
    ///
    /// The command MUST use non-zero exit status to indicate failures and print error message to stderr.
    /// It MUST print Transaction in JSON to stdout on success building.
    #[arg(
        name = "FIBER_FUNDING_TX_SHELL_BUILDER",
        long = "fiber-funding-tx-shell-builder",
        env,
        help = "Use an external shell command to build funding tx. [default: None]"
    )]
    #[cfg(not(target_arch = "wasm32"))]
    pub funding_tx_shell_builder: Option<String>,

    #[arg(skip)]
    #[cfg(target_arch = "wasm32")]
    pub wasm_secret_key: Option<SecretKey>,
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

    pub fn ckb_rpc_client(&self) -> CkbRpcAsyncClient {
        new_ckb_rpc_async_client(&self.rpc_url)
    }

    #[cfg(target_arch = "wasm32")]
    pub fn read_secret_key(&self) -> Result<SecretKey> {
        Ok(self.wasm_secret_key.expect("SecretKey not found on wasm"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn read_secret_key(&self) -> Result<SecretKey> {
        self.create_base_dir()?;
        let password = std::env::var(ENV_FIBER_SECRET_KEY_PASSWORD).map_err(|_| {
            crate::Error::SecretKeyFileError(format!(
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
                    .map_err(crate::Error::SecretKeyFileError)?;
                info!("secret key migration done");
            }
        }

        let key_bin =
            decrypt_from_file(&path, password_bytes).map_err(crate::Error::SecretKeyFileError)?;
        SecretKey::from_slice(&key_bin).map_err(|err| {
            crate::Error::SecretKeyFileError(format!("invalid secret key data, error: {}", err))
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

    #[cfg(not(target_arch = "wasm32"))]
    pub fn funding_tx_shell_builder_as_deref(&self) -> Option<&str> {
        self.funding_tx_shell_builder.as_deref()
    }

    #[cfg(target_arch = "wasm32")]
    #[doc(hidden)]
    pub fn funding_tx_shell_builder_as_deref(&self) -> Option<&str> {
        None
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

impl From<&ScriptCellDep> for UdtDep {
    fn from(value: &ScriptCellDep) -> Self {
        match value {
            ScriptCellDep::CellDep(cell_dep) => UdtDep::with_cell_dep(UdtCellDep::from(cell_dep)),
            ScriptCellDep::TypeID(type_id) => UdtDep::with_type_id(type_id.clone().into()),
        }
    }
}

impl UdtScript {
    pub fn allow_all_for_script(script: &Script) -> Self {
        Self {
            code_hash: H256(script.code_hash().as_slice().try_into().expect("32 bytes")),
            hash_type: script.hash_type().try_into().expect("valid hash type"),
            args: "0x.*".to_string(),
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

pub const CKB_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub fn new_ckb_rpc_async_client(rpc_url: &str) -> CkbRpcAsyncClient {
    #[cfg(not(target_arch = "wasm32"))]
    return CkbRpcAsyncClient::with_builder(rpc_url, |builder| builder.timeout(CKB_RPC_TIMEOUT))
        .expect("create ckb rpc client should not fail");
    #[cfg(target_arch = "wasm32")]
    return CkbRpcAsyncClient::new(rpc_url);
}

pub fn new_default_cell_collector(rpc_url: &str) -> DefaultCellCollector {
    #[cfg(not(target_arch = "wasm32"))]
    return DefaultCellCollector::new_with_timeout(rpc_url, CKB_RPC_TIMEOUT)
        .expect("create default cell collector should not fail");
    #[cfg(target_arch = "wasm32")]
    return DefaultCellCollector::new(rpc_url);
}

impl From<&CellDep> for UdtCellDep {
    fn from(cell_dep: &CellDep) -> Self {
        let index = cell_dep.out_point().index().unpack();
        UdtCellDep {
            dep_type: cell_dep.dep_type().try_into().expect("valid dep type"),
            out_point: OutPointWrapper {
                tx_hash: H256(
                    cell_dep
                        .out_point()
                        .tx_hash()
                        .as_slice()
                        .try_into()
                        .expect("32 bytes"),
                ),
                index,
            },
        }
    }
}
