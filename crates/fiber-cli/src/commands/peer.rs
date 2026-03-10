use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use fiber_json_types::{ConnectPeerParams, DisconnectPeerParams, ListPeersResult};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("peer")
        .about("Manage peer connections")
        .subcommand(ConnectPeerParams::augment_command(
            Command::new("connect_peer").about("Connect to a peer"),
        ))
        .subcommand(DisconnectPeerParams::augment_command(
            Command::new("disconnect_peer").about("Disconnect from a peer"),
        ))
        .subcommand(Command::new("list_peers").about("List connected peers"))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("connect_peer", sub)) => {
            let params = ConnectPeerParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("connect_peer", &params).await?;
            Ok(result)
        }
        Some(("disconnect_peer", sub)) => {
            let params = DisconnectPeerParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("disconnect_peer", &params).await?;
            Ok(result)
        }
        Some(("list_peers", _)) => {
            let result: ListPeersResult = client.call_typed_no_params("list_peers").await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown peer subcommand. Use --help")),
    }
}
