use clap_serde_derive::ClapSerde;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Clone)]
pub struct SocketAddress(pub lightning::ln::msgs::SocketAddress);

use serde::{Deserializer, Serializer};

impl<'s> From<&'s str> for SocketAddress {
    fn from(value: &'s str) -> Self {
        Self(lightning::ln::msgs::SocketAddress::from_str(value).expect("valid socket address"))
    }
}

impl serde::Serialize for SocketAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for SocketAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        lightning::ln::msgs::SocketAddress::from_str(&s)
            .map_err(serde::de::Error::custom)
            .map(Self)
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct Network(pub bitcoin::network::constants::Network);

impl<'s> From<&'s str> for Network {
    fn from(value: &'s str) -> Self {
        Self(bitcoin::network::constants::Network::from_str(value).expect("valid bitcoin network"))
    }
}

impl Default for Network {
    fn default() -> Self {
        Network(bitcoin::network::constants::Network::Testnet)
    }
}

#[derive(ClapSerde)]
pub struct LdkConfig {
    #[arg(long, env = "BITCOIND_RPC_USERNAME")]
    pub(crate) bitcoind_rpc_username: String,
    #[arg(long, env = "BITCOIND_RPC_PASSWORD")]
    pub(crate) bitcoind_rpc_password: String,
    #[arg(long, env = "BITCOIND_RPC_PORT")]
    pub(crate) bitcoind_rpc_port: u16,
    #[arg(long, env = "BITCOIND_RPC_HOST")]
    pub(crate) bitcoind_rpc_host: String,
    #[arg(long, env = "LDK_STORAGE_DIR_PATH")]
    pub(crate) ldk_storage_dir_path: String,
    #[arg(long, env = "LDK_PEER_LISTENING_PORT")]
    pub(crate) ldk_peer_listening_port: u16,
    #[arg(long, env = "LDK_ANNOUNCED_LISTEN_ADDR")]
    pub(crate) ldk_announced_listen_addr: Vec<SocketAddress>,
    // #[arg(long, env = "LDK_ANNOUNCED_NODE_NAME")]
    // pub(crate) ldk_announced_node_name: [u8; 32],
    #[arg(long, env = "BITCOIN_NETWORK")]
    pub(crate) bitcoin_network: Network,
}

impl LdkConfig {
    fn check() {}
}
