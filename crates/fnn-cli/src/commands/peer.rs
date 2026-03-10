use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use fiber_json_types::{ConnectPeerParams, DisconnectPeerParams, ListPeersResult, Pubkey};
use serde_json::Value;

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
            let address = sub.get_one::<String>("address").unwrap().clone();
            let save = sub
                .get_one::<String>("save")
                .map(|v| v.parse::<bool>().unwrap_or(true));

            let params = ConnectPeerParams {
                address: Some(address),
                pubkey: None,
                save,
            };
            let result: Value = client.call_typed("connect_peer", &params).await?;
            Ok(result)
        }
        Some(("disconnect_peer", sub)) => {
            let pubkey: Pubkey = sub
                .get_one::<String>("peer_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid peer_id: {}", e))?;
            let params = DisconnectPeerParams { pubkey };
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
