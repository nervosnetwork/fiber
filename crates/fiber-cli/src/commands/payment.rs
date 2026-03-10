use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use fiber_json_types::{
    BuildRouterParams, GetPaymentCommandParams, GetPaymentCommandResult, ListPaymentsParams,
    ListPaymentsResult, SendPaymentCommandParams, SendPaymentWithRouterParams,
};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("payment")
        .about("Manage payments")
        .subcommand(SendPaymentCommandParams::augment_command(
            Command::new("send_payment").about("Send a payment to a peer"),
        ))
        .subcommand(GetPaymentCommandParams::augment_command(
            Command::new("get_payment").about("Retrieve a payment by payment hash"),
        ))
        .subcommand(ListPaymentsParams::augment_command(
            Command::new("list_payments").about("List all payments"),
        ))
        .subcommand(BuildRouterParams::augment_command(
            Command::new("build_router").about("Build a payment router with specified hops"),
        ))
        .subcommand(SendPaymentWithRouterParams::augment_command(
            Command::new("send_payment_with_router")
                .about("Send a payment with a manually specified router"),
        ))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("send_payment", sub)) => {
            let params = SendPaymentCommandParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("send_payment", &params).await?;
            Ok(result)
        }
        Some(("get_payment", sub)) => {
            let params = GetPaymentCommandParams::from_arg_matches(sub)?;
            let result: GetPaymentCommandResult = client.call_typed("get_payment", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("list_payments", sub)) => {
            let params = ListPaymentsParams::from_arg_matches(sub)?;
            let result: ListPaymentsResult = client.call_typed("list_payments", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("build_router", sub)) => {
            let params = BuildRouterParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("build_router", &params).await?;
            Ok(result)
        }
        Some(("send_payment_with_router", sub)) => {
            let params = SendPaymentWithRouterParams::from_arg_matches(sub)?;
            let result: Value = client
                .call_typed("send_payment_with_router", &params)
                .await?;
            Ok(result)
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown payment subcommand. Use --help")),
    }
}
