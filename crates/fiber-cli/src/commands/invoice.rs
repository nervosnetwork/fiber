use crate::cli_generated::CliArgs;
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use fiber_json_types::{
    GetInvoiceResult, InvoiceParams, InvoiceResult, NewInvoiceParams, ParseInvoiceParams,
    ParseInvoiceResult, SettleInvoiceParams,
};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("invoice")
        .about("Manage invoices")
        .subcommand(NewInvoiceParams::augment_command(
            Command::new("new_invoice").about("Generate a new invoice"),
        ))
        .subcommand(ParseInvoiceParams::augment_command(
            Command::new("parse_invoice").about("Parse an encoded invoice"),
        ))
        .subcommand(InvoiceParams::augment_command(
            Command::new("get_invoice").about("Retrieve an invoice by payment hash"),
        ))
        .subcommand(InvoiceParams::augment_command(
            Command::new("cancel_invoice").about("Cancel an invoice"),
        ))
        .subcommand(SettleInvoiceParams::augment_command(
            Command::new("settle_invoice").about("Settle an invoice by providing the preimage"),
        ))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("new_invoice", sub)) => {
            let params = NewInvoiceParams::from_arg_matches(sub)?;
            let result: InvoiceResult = client.call_typed("new_invoice", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("parse_invoice", sub)) => {
            let params = ParseInvoiceParams::from_arg_matches(sub)?;
            let result: ParseInvoiceResult = client.call_typed("parse_invoice", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("get_invoice", sub)) => {
            let params = InvoiceParams::from_arg_matches(sub)?;
            let result: GetInvoiceResult = client.call_typed("get_invoice", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("cancel_invoice", sub)) => {
            let params = InvoiceParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("cancel_invoice", &params).await?;
            Ok(result)
        }
        Some(("settle_invoice", sub)) => {
            let params = SettleInvoiceParams::from_arg_matches(sub)?;
            let result: Value = client.call_typed("settle_invoice", &params).await?;
            Ok(result)
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown invoice subcommand. Use --help")),
    }
}
