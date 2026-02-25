use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{json, Value};

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
            let btc_pay_req = sub.get_one::<String>("btc_pay_req").unwrap();
            let currency = sub.get_one::<String>("currency").unwrap();
            let params = json!({
                "btc_pay_req": btc_pay_req,
                "currency": currency,
            });
            client.call_with_params("send_btc", params).await
        }
        Some(("receive_btc", sub)) => {
            let fiber_pay_req = sub.get_one::<String>("fiber_pay_req").unwrap();
            let params = json!({
                "fiber_pay_req": fiber_pay_req,
            });
            client.call_with_params("receive_btc", params).await
        }
        Some(("get_cch_order", sub)) => {
            let payment_hash = sub.get_one::<String>("payment_hash").unwrap();
            let params = json!({
                "payment_hash": payment_hash,
            });
            client.call_with_params("get_cch_order", params).await
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown cch subcommand. Use --help")),
    }
}
