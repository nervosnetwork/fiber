use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use fiber_json_types::{
    AddTlcParams, AddTlcResult, CheckChannelShutdownParams, CommitmentSignedParams, Hash256,
    RemoveTlcParams, RemoveTlcReason, SubmitCommitmentTransactionParams,
    SubmitCommitmentTransactionResult,
};
use serde_json::Value;

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
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let params = CommitmentSignedParams { channel_id };
            let result: Value = client.call_typed("commitment_signed", &params).await?;
            Ok(result)
        }
        Some(("add_tlc", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let amount: u128 = sub
                .get_one::<String>("amount")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid amount"))?;
            let payment_hash: Hash256 = sub
                .get_one::<String>("payment_hash")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;
            let expiry: u64 = sub
                .get_one::<String>("expiry")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid expiry"))?;
            let hash_algorithm = sub
                .get_one::<String>("hash_algorithm")
                .map(|s| serde_json::from_value(Value::String(s.clone())))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid hash_algorithm: {}", e))?;

            let params = AddTlcParams {
                channel_id,
                amount,
                payment_hash,
                expiry,
                hash_algorithm,
            };
            let result: AddTlcResult = client.call_typed("add_tlc", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("remove_tlc", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let tlc_id: u64 = sub
                .get_one::<String>("tlc_id")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid tlc_id"))?;
            let reason: RemoveTlcReason =
                serde_json::from_str(sub.get_one::<String>("reason").unwrap())
                    .map_err(|e| anyhow::anyhow!("Invalid reason JSON: {}", e))?;

            let params = RemoveTlcParams {
                channel_id,
                tlc_id,
                reason,
            };
            let result: Value = client.call_typed("remove_tlc", &params).await?;
            Ok(result)
        }
        Some(("submit_commitment_transaction", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let commitment_number: u64 = sub
                .get_one::<String>("commitment_number")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid commitment_number"))?;

            let params = SubmitCommitmentTransactionParams {
                channel_id,
                commitment_number,
            };
            let result: SubmitCommitmentTransactionResult = client
                .call_typed("submit_commitment_transaction", &params)
                .await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("check_channel_shutdown", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let params = CheckChannelShutdownParams { channel_id };
            let result: Value = client.call_typed("check_channel_shutdown", &params).await?;
            Ok(result)
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown dev subcommand. Use --help")),
    }
}
