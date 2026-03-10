use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use fiber_json_types::{
    CreatePreimageParams, CreateWatchChannelParams, Hash256, Privkey, Pubkey, RemovePreimageParams,
    RemoveWatchChannelParams, RevocationData, SettlementData, UpdateLocalSettlementParams,
    UpdatePendingRemoteSettlementParams, UpdateRevocationParams,
};
use serde_json::Value;

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
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let local_settlement_key: Privkey = sub
                .get_one::<String>("local_settlement_key")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid local_settlement_key: {}", e))?;
            let remote_settlement_key: Pubkey = sub
                .get_one::<String>("remote_settlement_key")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid remote_settlement_key: {}", e))?;
            let local_funding_pubkey: Pubkey = sub
                .get_one::<String>("local_funding_pubkey")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid local_funding_pubkey: {}", e))?;
            let remote_funding_pubkey: Pubkey = sub
                .get_one::<String>("remote_funding_pubkey")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid remote_funding_pubkey: {}", e))?;
            let settlement_data: SettlementData =
                serde_json::from_str(sub.get_one::<String>("settlement_data").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid settlement_data JSON: {}", e))?;
            let funding_udt_type_script = sub
                .get_one::<String>("funding_udt_type_script")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid funding_udt_type_script JSON: {}", e))?;

            let params = CreateWatchChannelParams {
                channel_id,
                local_settlement_key,
                remote_settlement_key,
                local_funding_pubkey,
                remote_funding_pubkey,
                settlement_data,
                funding_udt_type_script,
            };
            let result: Value = client.call_typed("create_watch_channel", &params).await?;
            Ok(result)
        }
        Some(("remove_watch_channel", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let params = RemoveWatchChannelParams { channel_id };
            let result: Value = client.call_typed("remove_watch_channel", &params).await?;
            Ok(result)
        }
        Some(("update_revocation", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let revocation_data: RevocationData =
                serde_json::from_str(sub.get_one::<String>("revocation_data").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid revocation_data JSON: {}", e))?;
            let settlement_data: SettlementData =
                serde_json::from_str(sub.get_one::<String>("settlement_data").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid settlement_data JSON: {}", e))?;

            let params = UpdateRevocationParams {
                channel_id,
                revocation_data,
                settlement_data,
            };
            let result: Value = client.call_typed("update_revocation", &params).await?;
            Ok(result)
        }
        Some(("update_pending_remote_settlement", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let settlement_data: SettlementData =
                serde_json::from_str(sub.get_one::<String>("settlement_data").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid settlement_data JSON: {}", e))?;

            let params = UpdatePendingRemoteSettlementParams {
                channel_id,
                settlement_data,
            };
            let result: Value = client
                .call_typed("update_pending_remote_settlement", &params)
                .await?;
            Ok(result)
        }
        Some(("update_local_settlement", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let settlement_data: SettlementData =
                serde_json::from_str(sub.get_one::<String>("settlement_data").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid settlement_data JSON: {}", e))?;

            let params = UpdateLocalSettlementParams {
                channel_id,
                settlement_data,
            };
            let result: Value = client
                .call_typed("update_local_settlement", &params)
                .await?;
            Ok(result)
        }
        Some(("create_preimage", sub)) => {
            let payment_hash: Hash256 = sub
                .get_one::<String>("payment_hash")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;
            let preimage: Hash256 = sub
                .get_one::<String>("preimage")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid preimage: {}", e))?;

            let params = CreatePreimageParams {
                payment_hash,
                preimage,
            };
            let result: Value = client.call_typed("create_preimage", &params).await?;
            Ok(result)
        }
        Some(("remove_preimage", sub)) => {
            let payment_hash: Hash256 = sub
                .get_one::<String>("payment_hash")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;

            let params = RemovePreimageParams { payment_hash };
            let result: Value = client.call_typed("remove_preimage", &params).await?;
            Ok(result)
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown watchtower subcommand. Use --help")),
    }
}
