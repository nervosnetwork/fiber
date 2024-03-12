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
    #[arg(long, env)]
    pub(crate) bitcoin_network: Network,
    #[arg(long, env)]
    pub(crate) bitcoind_rpc_username: String,
    #[arg(long, env)]
    pub(crate) bitcoind_rpc_password: String,
    #[arg(long, env)]
    pub(crate) bitcoind_rpc_port: u16,
    #[arg(long, env)]
    pub(crate) bitcoind_rpc_host: String,
    #[arg(long, env)]
    pub(crate) ldk_storage_dir_path: String,
    #[arg(long, env)]
    pub(crate) ldk_peer_listening_port: u16,
    #[arg(long, env)]
    pub(crate) ldk_announced_listen_addr: Vec<SocketAddress>,
    #[arg(long, env, value_parser = parse_announced_node_name)]
    pub(crate) ldk_announced_node_name: [u8; 32],
}

fn parse_announced_node_name(value: &str) -> Result<[u8; 32], String> {
    let str_bytes = value.as_bytes();
    if str_bytes.len() > 32 {
        return Err("Node Alias can not be longer than 32 bytes".to_string());
    }
    let mut bytes = [0; 32];
    bytes[..str_bytes.len()].copy_from_slice(str_bytes);
    Ok(bytes)
}

impl LdkConfig {
    fn check() {}
}
