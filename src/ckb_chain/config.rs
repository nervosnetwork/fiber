use clap_serde_derive::ClapSerde;

use secp256k1::SecretKey;
use std::{
    io::{ErrorKind, Read},
    path::PathBuf,
};

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
}
