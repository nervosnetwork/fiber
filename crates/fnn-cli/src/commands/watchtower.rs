use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{json, Value};

pub fn command() -> Command {
    Command::new("watchtower")
        .about("Watchtower service commands")
        .subcommand(
            Command::new("create_watch_channel")
                .about("Create a new watched channel")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID"),
                )
                .arg(
                    Arg::new("local_settlement_key")
                        .long("local-settlement-key")
                        .required(true)
                        .help("The local party's private key for settlement"),
                )
                .arg(
                    Arg::new("remote_settlement_key")
                        .long("remote-settlement-key")
                        .required(true)
                        .help("The remote party's public key for settlement"),
                )
                .arg(
                    Arg::new("local_funding_pubkey")
                        .long("local-funding-pubkey")
                        .required(true)
                        .help("The local party's funding public key"),
                )
                .arg(
                    Arg::new("remote_funding_pubkey")
                        .long("remote-funding-pubkey")
                        .required(true)
                        .help("The remote party's funding public key"),
                )
                .arg(
                    Arg::new("settlement_data")
                        .long("settlement-data")
                        .required(true)
                        .help("Settlement data as JSON object"),
                )
                .arg(
                    Arg::new("funding_udt_type_script")
                        .long("funding-udt-type-script")
                        .help("Funding UDT type script as JSON (optional)"),
                ),
        )
        .subcommand(
            Command::new("remove_watch_channel")
                .about("Remove a watched channel")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID"),
                ),
        )
        .subcommand(
            Command::new("update_revocation")
                .about("Update revocation data for a watched channel")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID"),
                )
                .arg(
                    Arg::new("revocation_data")
                        .long("revocation-data")
                        .required(true)
                        .help("Revocation data as JSON object"),
                )
                .arg(
                    Arg::new("settlement_data")
                        .long("settlement-data")
                        .required(true)
                        .help("Settlement data as JSON object"),
                ),
        )
        .subcommand(
            Command::new("update_pending_remote_settlement")
                .about("Update pending remote settlement data for a watched channel")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID"),
                )
                .arg(
                    Arg::new("settlement_data")
                        .long("settlement-data")
                        .required(true)
                        .help("Settlement data as JSON object"),
                ),
        )
        .subcommand(
            Command::new("update_local_settlement")
                .about("Update local settlement data for a watched channel")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID"),
                )
                .arg(
                    Arg::new("settlement_data")
                        .long("settlement-data")
                        .required(true)
                        .help("Settlement data as JSON object"),
                ),
        )
        .subcommand(
            Command::new("create_preimage")
                .about("Store a preimage associated with a payment hash")
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .required(true)
                        .help("The payment hash"),
                )
                .arg(
                    Arg::new("preimage")
                        .long("preimage")
                        .required(true)
                        .help("The preimage"),
                ),
        )
        .subcommand(
            Command::new("remove_preimage")
                .about("Remove a stored preimage by payment hash")
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .required(true)
                        .help("The payment hash"),
                ),
        )
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("create_watch_channel", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let local_settlement_key = sub.get_one::<String>("local_settlement_key").unwrap();
            let remote_settlement_key = sub.get_one::<String>("remote_settlement_key").unwrap();
            let local_funding_pubkey = sub.get_one::<String>("local_funding_pubkey").unwrap();
            let remote_funding_pubkey = sub.get_one::<String>("remote_funding_pubkey").unwrap();
            let settlement_data: Value =
                serde_json::from_str(sub.get_one::<String>("settlement_data").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid settlement_data JSON: {}", e))?;

            let mut params = json!({
                "channel_id": channel_id,
                "local_settlement_key": local_settlement_key,
                "remote_settlement_key": remote_settlement_key,
                "local_funding_pubkey": local_funding_pubkey,
                "remote_funding_pubkey": remote_funding_pubkey,
                "settlement_data": settlement_data,
            });

            if let Some(script) = sub.get_one::<String>("funding_udt_type_script") {
                params["funding_udt_type_script"] = serde_json::from_str(script)?;
            }

            client
                .call_with_params("create_watch_channel", params)
                .await
        }
        Some(("remove_watch_channel", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let params = json!({
                "channel_id": channel_id,
            });
            client
                .call_with_params("remove_watch_channel", params)
                .await
        }
        Some(("update_revocation", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let revocation_data: Value =
                serde_json::from_str(sub.get_one::<String>("revocation_data").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid revocation_data JSON: {}", e))?;
            let settlement_data: Value =
                serde_json::from_str(sub.get_one::<String>("settlement_data").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid settlement_data JSON: {}", e))?;

            let params = json!({
                "channel_id": channel_id,
                "revocation_data": revocation_data,
                "settlement_data": settlement_data,
            });
            client.call_with_params("update_revocation", params).await
        }
        Some(("update_pending_remote_settlement", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let settlement_data: Value =
                serde_json::from_str(sub.get_one::<String>("settlement_data").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid settlement_data JSON: {}", e))?;

            let params = json!({
                "channel_id": channel_id,
                "settlement_data": settlement_data,
            });
            client
                .call_with_params("update_pending_remote_settlement", params)
                .await
        }
        Some(("update_local_settlement", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let settlement_data: Value =
                serde_json::from_str(sub.get_one::<String>("settlement_data").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid settlement_data JSON: {}", e))?;

            let params = json!({
                "channel_id": channel_id,
                "settlement_data": settlement_data,
            });
            client
                .call_with_params("update_local_settlement", params)
                .await
        }
        Some(("create_preimage", sub)) => {
            let payment_hash = sub.get_one::<String>("payment_hash").unwrap();
            let preimage = sub.get_one::<String>("preimage").unwrap();
            let params = json!({
                "payment_hash": payment_hash,
                "preimage": preimage,
            });
            client.call_with_params("create_preimage", params).await
        }
        Some(("remove_preimage", sub)) => {
            let payment_hash = sub.get_one::<String>("payment_hash").unwrap();
            let params = json!({
                "payment_hash": payment_hash,
            });
            client.call_with_params("remove_preimage", params).await
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown watchtower subcommand. Use --help")),
    }
}
