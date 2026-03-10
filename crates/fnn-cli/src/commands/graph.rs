use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use fiber_json_types::{
    GraphChannelsParams, GraphChannelsResult, GraphNodesParams, GraphNodesResult,
};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("graph")
        .about("Query the network graph")
        .subcommand(GraphNodesParams::augment_command(
            Command::new("graph_nodes").about("Get the list of nodes in the network graph"),
        ))
        .subcommand(GraphChannelsParams::augment_command(
            Command::new("graph_channels").about("Get the list of channels in the network graph"),
        ))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("graph_nodes", sub)) => {
            let params = GraphNodesParams::from_arg_matches(sub)?;
            let result: GraphNodesResult = client.call_typed("graph_nodes", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("graph_channels", sub)) => {
            let params = GraphChannelsParams::from_arg_matches(sub)?;
            let result: GraphChannelsResult = client.call_typed("graph_channels", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown graph subcommand. Use --help")),
    }
}
