use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use fiber_json_types::{
    AbandonChannelParams, AcceptChannelParams, ListChannelsParams, ListChannelsResult,
    OpenChannelParams, OpenChannelResult, ShutdownChannelParams, UpdateChannelParams,
};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("channel")
        .about("Manage payment channels")
        .subcommand(OpenChannelParams::augment_command(
            Command::new("open_channel").about("Open a channel with a peer"),
        ))
        .subcommand(AcceptChannelParams::augment_command(
            Command::new("accept_channel").about("Accept a channel opening request"),
        ))
        .subcommand(AbandonChannelParams::augment_command(
            Command::new("abandon_channel").about("Abandon a channel"),
        ))
        .subcommand(ListChannelsParams::augment_command(
            Command::new("list_channels").about("List all channels"),
        ))
        .subcommand(ShutdownChannelParams::augment_command(
            Command::new("shutdown_channel").about("Shut down a channel"),
        ))
        .subcommand(UpdateChannelParams::augment_command(
            Command::new("update_channel").about("Update a channel's parameters"),
        ))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("open_channel", sub)) => {
            let params = OpenChannelParams::from_arg_matches(sub)?;
            let result: OpenChannelResult = client.call_typed("open_channel", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("accept_channel", sub)) => {
            let params = AcceptChannelParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("accept_channel", &params).await?;
            Ok(result)
        }
        Some(("abandon_channel", sub)) => {
            let params = AbandonChannelParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("abandon_channel", &params).await?;
            Ok(result)
        }
        Some(("list_channels", sub)) => {
            let params = ListChannelsParams::from_arg_matches(sub)?;
            let result: ListChannelsResult = client.call_typed("list_channels", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("shutdown_channel", sub)) => {
            let params = ShutdownChannelParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("shutdown_channel", &params).await?;
            Ok(result)
        }
        Some(("update_channel", sub)) => {
            let params = UpdateChannelParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("update_channel", &params).await?;
            Ok(result)
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown channel subcommand. Use --help")),
    }
}
