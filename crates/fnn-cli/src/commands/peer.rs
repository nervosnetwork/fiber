use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{json, Value};

pub fn command() -> Command {
    Command::new("peer")
        .about("Manage peer connections")
        .subcommand(
            Command::new("connect_peer")
                .about("Connect to a peer")
                .arg(
                    Arg::new("address")
                        .long("address")
                        .required(true)
                        .help("The multi-address of the peer to connect to"),
                )
                .arg(
                    Arg::new("save")
                        .long("save")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Whether to save the peer address to the peer store"),
                ),
        )
        .subcommand(
            Command::new("disconnect_peer")
                .about("Disconnect from a peer")
                .arg(
                    Arg::new("peer_id")
                        .long("peer-id")
                        .required(true)
                        .help("The peer ID of the peer to disconnect"),
                ),
        )
        .subcommand(Command::new("list_peers").about("List connected peers"))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("connect_peer", sub)) => {
            let address = sub.get_one::<String>("address").unwrap();
            let save = sub
                .get_one::<String>("save")
                .map(|v| v.parse::<bool>().unwrap_or(true));

            let mut params = json!({
                "address": address,
            });
            if let Some(save) = save {
                params["save"] = json!(save);
            }

            client.call_with_params("connect_peer", params).await
        }
        Some(("disconnect_peer", sub)) => {
            let peer_id = sub.get_one::<String>("peer_id").unwrap();
            let params = json!({
                "peer_id": peer_id,
            });
            client.call_with_params("disconnect_peer", params).await
        }
        Some(("list_peers", _)) => client.call_no_params("list_peers").await,
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown peer subcommand. Use --help")),
    }
}
