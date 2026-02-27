use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("info")
        .about("Get node information")
        .subcommand(Command::new("node_info").about("Get the node information"))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("node_info", _)) | None => {
            // Default to node_info when no subcommand specified
            client.call_no_params("node_info").await
        }
        _ => Err(anyhow::anyhow!("Unknown info subcommand")),
    }
}
