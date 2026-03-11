use crate::cli_generated::CliArgs;
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use fiber_json_types::{CchOrderResponse, GetCchOrderParams, ReceiveBTCParams, SendBTCParams};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("cch")
        .about("Cross-chain hub operations")
        .subcommand(SendBTCParams::augment_command(
            Command::new("send_btc").about("Create a CCH order for a BTC Lightning payee"),
        ))
        .subcommand(ReceiveBTCParams::augment_command(
            Command::new("receive_btc").about("Create a CCH order for a CKB Fiber payee"),
        ))
        .subcommand(GetCchOrderParams::augment_command(
            Command::new("get_cch_order").about("Get a CCH order by payment hash"),
        ))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("send_btc", sub)) => {
            let params = SendBTCParams::from_arg_matches(sub)?;
            let result: CchOrderResponse = client.call_typed("send_btc", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("receive_btc", sub)) => {
            let params = ReceiveBTCParams::from_arg_matches(sub)?;
            let result: CchOrderResponse = client.call_typed("receive_btc", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("get_cch_order", sub)) => {
            let params = GetCchOrderParams::from_arg_matches(sub)?;
            let result: CchOrderResponse = client.call_typed("get_cch_order", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown cch subcommand. Use --help")),
    }
}
