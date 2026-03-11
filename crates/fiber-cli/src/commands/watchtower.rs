use crate::cli_generated::CliArgs;
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use fiber_json_types::{
    CreatePreimageParams, CreateWatchChannelParams, RemovePreimageParams, RemoveWatchChannelParams,
    UpdateLocalSettlementParams, UpdatePendingRemoteSettlementParams, UpdateRevocationParams,
};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("watchtower")
        .about("Watchtower service commands")
        .subcommand(CreateWatchChannelParams::augment_command(
            Command::new("create_watch_channel").about("Create a new watched channel"),
        ))
        .subcommand(RemoveWatchChannelParams::augment_command(
            Command::new("remove_watch_channel").about("Remove a watched channel"),
        ))
        .subcommand(UpdateRevocationParams::augment_command(
            Command::new("update_revocation").about("Update revocation data for a watched channel"),
        ))
        .subcommand(UpdatePendingRemoteSettlementParams::augment_command(
            Command::new("update_pending_remote_settlement")
                .about("Update pending remote settlement data for a watched channel"),
        ))
        .subcommand(UpdateLocalSettlementParams::augment_command(
            Command::new("update_local_settlement")
                .about("Update local settlement data for a watched channel"),
        ))
        .subcommand(CreatePreimageParams::augment_command(
            Command::new("create_preimage")
                .about("Store a preimage associated with a payment hash"),
        ))
        .subcommand(RemovePreimageParams::augment_command(
            Command::new("remove_preimage").about("Remove a stored preimage by payment hash"),
        ))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("create_watch_channel", sub)) => {
            let params = CreateWatchChannelParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("create_watch_channel", &params).await?;
            Ok(result)
        }
        Some(("remove_watch_channel", sub)) => {
            let params = RemoveWatchChannelParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("remove_watch_channel", &params).await?;
            Ok(result)
        }
        Some(("update_revocation", sub)) => {
            let params = UpdateRevocationParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("update_revocation", &params).await?;
            Ok(result)
        }
        Some(("update_pending_remote_settlement", sub)) => {
            let params = UpdatePendingRemoteSettlementParams::from_arg_matches(sub)?;
            let result: Value = client
                .call_typed("update_pending_remote_settlement", &params)
                .await?;
            Ok(result)
        }
        Some(("update_local_settlement", sub)) => {
            let params = UpdateLocalSettlementParams::from_arg_matches(sub)?;
            let result: Value = client
                .call_typed("update_local_settlement", &params)
                .await?;
            Ok(result)
        }
        Some(("create_preimage", sub)) => {
            let params = CreatePreimageParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("create_preimage", &params).await?;
            Ok(result)
        }
        Some(("remove_preimage", sub)) => {
            let params = RemovePreimageParams::from_arg_matches(sub)?;
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
