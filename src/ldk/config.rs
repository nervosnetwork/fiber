use clap_serde_derive::ClapSerde;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, PartialEq, Clone)]
pub struct SocketAddress(pub lightning::ln::msgs::SocketAddress);

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

// The prefix `ldk-`/`LDK_` is somewhat redundant here. It serves as some kind of namespace.
// as we cram ckb and ldk config into the same clap `Command` argument space with `flatten`.
// Both name and long options are required as both of them need to be unique across the same clap `Command`.
// We have to do so manually as upstream does not support such automatically adding prefix feature.
// [derive feature: an attribute to add a prefix to all arg names in a struct, for use with flatten · Issue #3513 · clap-rs/clap](https://github.com/clap-rs/clap/issues/3513)
#[derive(ClapSerde, Debug, Clone)]
pub struct LdkConfig {
    /// bitcoin network
    #[arg(
        name = "LDK_BITCOIN_NETWORK",
        long = "ldk-bitcoin-network",
        env,
        default_value = "testnet"
    )]
    pub(crate) bitcoin_network: Network,

    /// bitcoin rpc username
    #[arg(
        name = "LDK_BITCOIN_RPC_USERNAME",
        long = "ldk-bitcoin-rpc-username",
        env
    )]
    pub(crate) bitcoin_rpc_username: String,

    /// bitcoin rpc password
    #[arg(
        name = "LDK_BITCOIN_RPC_PASSWORD",
        long = "ldk-bitcoin-rpc-password",
        env
    )]
    pub(crate) bitcoin_rpc_password: String,

    /// bitcoin rpc service port
    #[arg(
        name = "LDK_BITCOIN_RPC_PORT",
        long = "ldk-bitcoin-rpc-port",
        env,
        default_value = "18332"
    )]
    pub(crate) bitcoin_rpc_port: u16,

    /// bitcoin rpc service hostname
    #[arg(
        name = "LDK_BITCOIN_RPC_HOST",
        long = "ldk-bitcoin-rpc-host",
        env,
        default_value = "127.0.0.1"
    )]
    pub(crate) bitcoin_rpc_host: String,

    /// ldk base directory
    #[arg(
        name = "LDK_BASE_DIR",
        long = "ldk-base-dir",
        env,
        help = "base directory for ldk [default: $BASE_DIR/ldk]"
    )]
    pub(crate) base_dir: Option<PathBuf>,

    /// listening port of bitcoin lightning network
    #[arg(name = "LDK_LISTENING_PORT", long = "ldk-listening-port", env)]
    pub(crate) listening_port: u16,

    /// addresses to be announced to lightning network (separated by `,`)
    #[arg(name =  "LDK_ANNOUNCED_LISTEN_ADDR", long = "ldk-announced-listen-addr", env, value_parser, num_args = 0.., value_delimiter = ',')]
    pub(crate) announced_listen_addr: Vec<SocketAddress>,

    /// node name to be announced to lightning network
    #[arg(
        name = "LDK_ANNOUNCED_NODE_NAME",
        long = "ldk-announced-node-name",
        env
    )]
    pub(crate) announced_node_name: AnnouncedNodeName,
}

impl LdkConfig {
    pub fn base_dir(&self) -> &PathBuf {
        self.base_dir.as_ref().expect("have set base dir")
    }
}
