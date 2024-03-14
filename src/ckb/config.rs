use std::{fs, path::PathBuf};

use clap_serde_derive::{
    clap::{self},
    ClapSerde,
};

use crate::config::get_default_ckb_dir;
use crate::Result;

// See comment in `LdkConfig` for why do we need to specify both name and long,
// and prefix them with `ckb-`/`CKB_`.
#[derive(ClapSerde, Debug, Clone)]
pub struct CkbConfig {
    #[arg(name = "CKB_STORAGE_DIR", long = "ckb-storage-dir", env, default_value = get_default_ckb_dir().into_os_string())]
    pub(crate) storage_dir: PathBuf,
    #[arg(
        name = "CKB_PEER_LISTENING_PORT",
        long = "ckb-peer-listening-port",
        env
    )]
    pub(crate) peer_listening_port: u16,
    #[arg(name = "CKB_ANNOUNCED_LISTEN_ADDR", long = "ckb-announced-listen-addr", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub(crate) announced_listen_addr: Vec<String>,
    #[arg(
        name = "CKB_ANNOUNCED_NODE_NAME",
        long = "ckb-announced-node-name",
        env
    )]
    pub(crate) announced_node_name: String,
}

impl CkbConfig {
    pub fn create_storage_dir(&self) -> Result<()> {
        if !self.storage_dir.exists() {
            fs::create_dir(&self.storage_dir).map_err(Into::into)
        } else {
            Ok(())
        }
    }

    pub fn read_or_generate_secret_key(&self) -> Result<super::KeyPair> {
        self.create_storage_dir()?;
        super::key::read_or_generate_private_key(&self.storage_dir.join("sk")).map_err(Into::into)
    }
}
