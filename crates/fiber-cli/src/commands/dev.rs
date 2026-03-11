use crate::cli_generated::CliArgs;
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use fiber_json_types::{
    AddTlcParams, AddTlcResult, CheckChannelShutdownParams, CommitmentSignedParams,
    RemoveTlcParams, SubmitCommitmentTransactionParams, SubmitCommitmentTransactionResult,
};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("dev")
        .about("Development/debug commands (not for production use)")
        .subcommand(CommitmentSignedParams::augment_command(
            Command::new("commitment_signed").about("Send a commitment_signed message to a peer"),
        ))
        .subcommand(AddTlcParams::augment_command(
            Command::new("add_tlc").about("Add a TLC to a channel"),
        ))
        .subcommand(RemoveTlcParams::augment_command(
            Command::new("remove_tlc").about("Remove a TLC from a channel"),
        ))
        .subcommand(SubmitCommitmentTransactionParams::augment_command(
            Command::new("submit_commitment_transaction")
                .about("Submit a commitment transaction to the chain"),
        ))
        .subcommand(CheckChannelShutdownParams::augment_command(
            Command::new("check_channel_shutdown")
                .about("Manually trigger CheckShutdownTx on a channel"),
        ))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("commitment_signed", sub)) => {
            let params = CommitmentSignedParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("commitment_signed", &params).await?;
            Ok(result)
        }
        Some(("add_tlc", sub)) => {
            let params = AddTlcParams::from_arg_matches(sub)?;
            let result: AddTlcResult = client.call_typed("add_tlc", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("remove_tlc", sub)) => {
            let params = RemoveTlcParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("remove_tlc", &params).await?;
            Ok(result)
        }
        Some(("submit_commitment_transaction", sub)) => {
            let params = SubmitCommitmentTransactionParams::from_arg_matches(sub)?;
            let result: SubmitCommitmentTransactionResult = client
                .call_typed("submit_commitment_transaction", &params)
                .await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("check_channel_shutdown", sub)) => {
            let params = CheckChannelShutdownParams::from_arg_matches(sub)?;
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
