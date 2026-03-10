use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use fiber_json_types::{
    CchOrderResponse, GetCchOrderParams, Hash256, ReceiveBTCParams, SendBTCParams,
};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("cch")
        .about("Cross-chain hub operations")
        .subcommand(
            Command::new("send_btc")
                .about("Create a CCH order for a BTC Lightning payee")
                .arg(
                    Arg::new("btc_pay_req")
                        .long("btc-pay-req")
                        .required(true)
                        .help("Payment request string for the BTC Lightning payee"),
                )
                .arg(
                    Arg::new("currency")
                        .long("currency")
                        .required(true)
                        .help("Request currency (Fibb, Fibt, Fibd)"),
                ),
        )
        .subcommand(
            Command::new("receive_btc")
                .about("Create a CCH order for a CKB Fiber payee")
                .arg(
                    Arg::new("fiber_pay_req")
                        .long("fiber-pay-req")
                        .required(true)
                        .help("Payment request string for the CKB Fiber payee"),
                ),
        )
        .subcommand(
            Command::new("get_cch_order")
                .about("Get a CCH order by payment hash")
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .required(true)
                        .help("Payment hash for the HTLC"),
                ),
        )
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("send_btc", sub)) => {
            let btc_pay_req = sub.get_one::<String>("btc_pay_req").unwrap().clone();
            let currency = serde_json::from_value(Value::String(
                sub.get_one::<String>("currency").unwrap().clone(),
            ))
            .map_err(|e| anyhow::anyhow!("Invalid currency: {}", e))?;

            let params = SendBTCParams {
                btc_pay_req,
                currency,
            };
            let result: CchOrderResponse = client.call_typed("send_btc", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("receive_btc", sub)) => {
            let fiber_pay_req = sub.get_one::<String>("fiber_pay_req").unwrap().clone();
            let params = ReceiveBTCParams { fiber_pay_req };
            let result: CchOrderResponse = client.call_typed("receive_btc", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("get_cch_order", sub)) => {
            let payment_hash: Hash256 = sub
                .get_one::<String>("payment_hash")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;
            let params = GetCchOrderParams { payment_hash };
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
