use clap_serde_derive::ClapSerde;
use std::path::PathBuf;
use std::str::FromStr;

use crate::config::get_default_ldk_dir;

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

#[derive(Debug, PartialEq, Copy, Clone, Default)]
pub struct AnnouncedNodeName(pub [u8; 32]);

fn parse_announced_node_name(value: &str) -> Result<[u8; 32], String> {
    let str_bytes = value.as_bytes();
    if str_bytes.len() > 32 {
        return Err("Node Alias can not be longer than 32 bytes".to_string());
    }
    let mut bytes = [0; 32];
    bytes[..str_bytes.len()].copy_from_slice(str_bytes);
    Ok(bytes)
}

impl<'s> From<&'s str> for AnnouncedNodeName {
    fn from(value: &'s str) -> Self {
        Self(parse_announced_node_name(value).expect("Valid announced node name"))
    }
}

impl serde::Serialize for AnnouncedNodeName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(std::str::from_utf8(&self.0).expect("valid utf8 string"))
    }
}

impl<'de> serde::Deserialize<'de> for AnnouncedNodeName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_announced_node_name(&s)
            .map_err(serde::de::Error::custom)
            .map(Self)
    }
}

// The prefix `ldk_` is somewhat redundant here. It serves as some kind of namespace.
// [derive feature: an attribute to add a prefix to all arg names in a struct, for use with flatten · Issue #3513 · clap-rs/clap](https://github.com/clap-rs/clap/issues/3513)
#[derive(ClapSerde, Debug)]
pub struct LdkConfig {
    #[arg(long, env)]
    pub(crate) ldk_bitcoin_network: Network,
    #[arg(long, env)]
    pub(crate) ldk_bitcoin_rpc_username: String,
    #[arg(long, env)]
    pub(crate) ldk_bitcoin_rpc_password: String,
    #[arg(long, env)]
    pub(crate) ldk_bitcoin_rpc_port: u16,
    #[arg(long, env)]
    pub(crate) ldk_bitcoin_rpc_host: String,
    #[arg(long, env, default_value=get_default_ldk_dir().into_os_string())]
    pub(crate) ldk_storage_dir_path: PathBuf,
    #[arg(long, env)]
    pub(crate) ldk_peer_listening_port: u16,
    #[arg(long, env)]
    pub(crate) ldk_announced_listen_addr: Vec<SocketAddress>,
    #[arg(long, env)]
    pub(crate) ldk_announced_node_name: AnnouncedNodeName,
}
