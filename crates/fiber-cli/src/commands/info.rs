use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use fiber_json_types::NodeInfoResult;
use serde_json::Value;

pub fn command() -> Command {
    Command::new("info")
        .about("Get node information")
        .subcommand(Command::new("node_info").about("Get the node information"))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("node_info", _)) | None => {
            let result: NodeInfoResult = client.call_typed_no_params("node_info").await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        _ => Err(anyhow::anyhow!("Unknown info subcommand")),
    }
}
