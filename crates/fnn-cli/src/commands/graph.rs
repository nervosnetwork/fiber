use crate::rpc_client::RpcClient;
use anyhow::Result;
use ckb_jsonrpc_types::JsonBytes;
use clap::{Arg, ArgMatches, Command};
use fiber_json_types::{
    GraphChannelsParams, GraphChannelsResult, GraphNodesParams, GraphNodesResult,
};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("graph")
        .about("Query the network graph")
        .subcommand(
            Command::new("graph_nodes")
                .about("Get the list of nodes in the network graph")
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .help("The maximum number of nodes to return"),
                )
                .arg(
                    Arg::new("after")
                        .long("after")
                        .help("The cursor to start returning nodes from"),
                ),
        )
        .subcommand(
            Command::new("graph_channels")
                .about("Get the list of channels in the network graph")
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .help("The maximum number of channels to return"),
                )
                .arg(
                    Arg::new("after")
                        .long("after")
                        .help("The cursor to start returning channels from"),
                ),
        )
}

fn parse_optional_u64(sub: &ArgMatches, name: &str) -> Result<Option<u64>> {
    sub.get_one::<String>(name)
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| anyhow::anyhow!("Invalid {}", name))
        })
        .transpose()
}

fn parse_after_cursor(sub: &ArgMatches) -> Result<Option<JsonBytes>> {
    sub.get_one::<String>("after")
        .map(|v| {
            serde_json::from_value(Value::String(v.clone()))
                .map_err(|e| anyhow::anyhow!("Invalid after cursor: {}", e))
        })
        .transpose()
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("graph_nodes", sub)) => {
            let limit = parse_optional_u64(sub, "limit")?;
            let after = parse_after_cursor(sub)?;

            let params = GraphNodesParams { limit, after };
            let result: GraphNodesResult = client.call_typed("graph_nodes", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("graph_channels", sub)) => {
            let limit = parse_optional_u64(sub, "limit")?;
            let after = parse_after_cursor(sub)?;

            let params = GraphChannelsParams { limit, after };
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
