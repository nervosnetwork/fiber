use super::utils::to_hex_u64;
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{json, Value};

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

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("graph_nodes", sub)) => {
            let mut params = json!({});

            if let Some(v) = sub.get_one::<String>("limit") {
                let val: u64 = v.parse().map_err(|_| anyhow::anyhow!("Invalid limit"))?;
                params["limit"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("after") {
                params["after"] = json!(v);
            }

            client.call_with_params("graph_nodes", params).await
        }
        Some(("graph_channels", sub)) => {
            let mut params = json!({});

            if let Some(v) = sub.get_one::<String>("limit") {
                let val: u64 = v.parse().map_err(|_| anyhow::anyhow!("Invalid limit"))?;
                params["limit"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("after") {
                params["after"] = json!(v);
            }

            client.call_with_params("graph_channels", params).await
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown graph subcommand. Use --help")),
    }
}
