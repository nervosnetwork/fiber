use super::utils::{to_hex_u128, to_hex_u64};
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{json, Value};

pub fn command() -> Command {
    Command::new("dev")
        .about("Development/debug commands (not for production use)")
        .subcommand(
            Command::new("commitment_signed")
                .about("Send a commitment_signed message to a peer")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID"),
                ),
        )
        .subcommand(
            Command::new("add_tlc")
                .about("Add a TLC to a channel")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID"),
                )
                .arg(
                    Arg::new("amount")
                        .long("amount")
                        .required(true)
                        .help("The amount of the TLC"),
                )
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .required(true)
                        .help("The payment hash of the TLC"),
                )
                .arg(
                    Arg::new("expiry")
                        .long("expiry")
                        .required(true)
                        .help("The expiry of the TLC (in milliseconds)"),
                )
                .arg(
                    Arg::new("hash_algorithm")
                        .long("hash-algorithm")
                        .help("The hash algorithm: CkbHash or Sha256"),
                ),
        )
        .subcommand(
            Command::new("remove_tlc")
                .about("Remove a TLC from a channel")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID"),
                )
                .arg(
                    Arg::new("tlc_id")
                        .long("tlc-id")
                        .required(true)
                        .help("The ID of the TLC to remove"),
                )
                .arg(
                    Arg::new("reason")
                        .long("reason")
                        .required(true)
                        .help("The reason for removing the TLC (JSON object, e.g. {\"payment_preimage\":\"0x...\"} or {\"error_code\":\"0x..\"})"),
                ),
        )
        .subcommand(
            Command::new("submit_commitment_transaction")
                .about("Submit a commitment transaction to the chain")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID"),
                )
                .arg(
                    Arg::new("commitment_number")
                        .long("commitment-number")
                        .required(true)
                        .help("The commitment number"),
                ),
        )
        .subcommand(
            Command::new("check_channel_shutdown")
                .about("Manually trigger CheckShutdownTx on a channel")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID"),
                ),
        )
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("commitment_signed", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let params = json!({
                "channel_id": channel_id,
            });
            client.call_with_params("commitment_signed", params).await
        }
        Some(("add_tlc", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let amount: u128 = sub
                .get_one::<String>("amount")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid amount"))?;
            let payment_hash = sub.get_one::<String>("payment_hash").unwrap();
            let expiry: u64 = sub
                .get_one::<String>("expiry")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid expiry"))?;

            let mut params = json!({
                "channel_id": channel_id,
                "amount": to_hex_u128(amount),
                "payment_hash": payment_hash,
                "expiry": to_hex_u64(expiry),
            });

            if let Some(algo) = sub.get_one::<String>("hash_algorithm") {
                params["hash_algorithm"] = json!(algo);
            }

            client.call_with_params("add_tlc", params).await
        }
        Some(("remove_tlc", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let tlc_id: u64 = sub
                .get_one::<String>("tlc_id")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid tlc_id"))?;
            let reason: Value = serde_json::from_str(sub.get_one::<String>("reason").unwrap())
                .map_err(|e| anyhow::anyhow!("Invalid reason JSON: {}", e))?;

            let params = json!({
                "channel_id": channel_id,
                "tlc_id": to_hex_u64(tlc_id),
                "reason": reason,
            });
            client.call_with_params("remove_tlc", params).await
        }
        Some(("submit_commitment_transaction", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let commitment_number: u64 = sub
                .get_one::<String>("commitment_number")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid commitment_number"))?;

            let params = json!({
                "channel_id": channel_id,
                "commitment_number": to_hex_u64(commitment_number),
            });
            client
                .call_with_params("submit_commitment_transaction", params)
                .await
        }
        Some(("check_channel_shutdown", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let params = json!({
                "channel_id": channel_id,
            });
            client
                .call_with_params("check_channel_shutdown", params)
                .await
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown dev subcommand. Use --help")),
    }
}
