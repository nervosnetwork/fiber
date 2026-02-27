use super::utils::{to_hex_u128, to_hex_u64};
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{json, Value};

pub fn command() -> Command {
    Command::new("invoice")
        .about("Manage invoices")
        .subcommand(
            Command::new("new_invoice")
                .about("Generate a new invoice")
                .arg(
                    Arg::new("amount")
                        .long("amount")
                        .required(true)
                        .help("The amount of the invoice (in Shannons)"),
                )
                .arg(
                    Arg::new("currency")
                        .long("currency")
                        .required(true)
                        .help("The currency of the invoice (Fibb, Fibt, Fibd)"),
                )
                .arg(
                    Arg::new("description")
                        .long("description")
                        .help("The description of the invoice"),
                )
                .arg(
                    Arg::new("payment_preimage")
                        .long("payment-preimage")
                        .help("The preimage to settle an incoming TLC (hex)"),
                )
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .help("The hash of the preimage for a hold invoice (hex)"),
                )
                .arg(
                    Arg::new("expiry")
                        .long("expiry")
                        .help("The expiry time of the invoice in seconds"),
                )
                .arg(
                    Arg::new("fallback_address")
                        .long("fallback-address")
                        .help("The fallback address of the invoice"),
                )
                .arg(
                    Arg::new("final_expiry_delta")
                        .long("final-expiry-delta")
                        .help("The final HTLC timeout in milliseconds"),
                )
                .arg(
                    Arg::new("udt_type_script")
                        .long("udt-type-script")
                        .help("The UDT type script of the invoice (JSON)"),
                )
                .arg(
                    Arg::new("hash_algorithm")
                        .long("hash-algorithm")
                        .help("The hash algorithm (CkbHash or Sha256)"),
                )
                .arg(
                    Arg::new("allow_mpp")
                        .long("allow-mpp")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Whether to allow multi-part payments"),
                )
                .arg(
                    Arg::new("allow_trampoline_routing")
                        .long("allow-trampoline-routing")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Whether to allow trampoline routing"),
                ),
        )
        .subcommand(
            Command::new("parse_invoice")
                .about("Parse an encoded invoice")
                .arg(
                    Arg::new("invoice")
                        .long("invoice")
                        .required(true)
                        .help("The encoded invoice address string"),
                ),
        )
        .subcommand(
            Command::new("get_invoice")
                .about("Retrieve an invoice by payment hash")
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .required(true)
                        .help("The payment hash of the invoice"),
                ),
        )
        .subcommand(
            Command::new("cancel_invoice")
                .about("Cancel an invoice")
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .required(true)
                        .help("The payment hash of the invoice to cancel"),
                ),
        )
        .subcommand(
            Command::new("settle_invoice")
                .about("Settle an invoice by providing the preimage")
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .required(true)
                        .help("The payment hash of the invoice"),
                )
                .arg(
                    Arg::new("payment_preimage")
                        .long("payment-preimage")
                        .required(true)
                        .help("The payment preimage"),
                ),
        )
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("new_invoice", sub)) => {
            let amount: u128 = sub
                .get_one::<String>("amount")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid amount"))?;
            let currency = sub.get_one::<String>("currency").unwrap();

            let mut params = json!({
                "amount": to_hex_u128(amount),
                "currency": currency,
            });

            if let Some(desc) = sub.get_one::<String>("description") {
                params["description"] = json!(desc);
            }
            if let Some(v) = sub.get_one::<String>("payment_preimage") {
                params["payment_preimage"] = json!(v);
            }
            if let Some(v) = sub.get_one::<String>("payment_hash") {
                params["payment_hash"] = json!(v);
            }
            if let Some(v) = sub.get_one::<String>("expiry") {
                let val: u64 = v.parse().map_err(|_| anyhow::anyhow!("Invalid expiry"))?;
                params["expiry"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("fallback_address") {
                params["fallback_address"] = json!(v);
            }
            if let Some(v) = sub.get_one::<String>("final_expiry_delta") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid final_expiry_delta"))?;
                params["final_expiry_delta"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("udt_type_script") {
                params["udt_type_script"] = serde_json::from_str(v)?;
            }
            if let Some(v) = sub.get_one::<String>("hash_algorithm") {
                params["hash_algorithm"] = json!(v);
            }
            if let Some(v) = sub.get_one::<String>("allow_mpp") {
                params["allow_mpp"] = json!(v.parse::<bool>().unwrap_or(true));
            }
            if let Some(v) = sub.get_one::<String>("allow_trampoline_routing") {
                params["allow_trampoline_routing"] = json!(v.parse::<bool>().unwrap_or(true));
            }

            client.call_with_params("new_invoice", params).await
        }
        Some(("parse_invoice", sub)) => {
            let invoice = sub.get_one::<String>("invoice").unwrap();
            let params = json!({
                "invoice": invoice,
            });
            client.call_with_params("parse_invoice", params).await
        }
        Some(("get_invoice", sub)) => {
            let payment_hash = sub.get_one::<String>("payment_hash").unwrap();
            let params = json!({
                "payment_hash": payment_hash,
            });
            client.call_with_params("get_invoice", params).await
        }
        Some(("cancel_invoice", sub)) => {
            let payment_hash = sub.get_one::<String>("payment_hash").unwrap();
            let params = json!({
                "payment_hash": payment_hash,
            });
            client.call_with_params("cancel_invoice", params).await
        }
        Some(("settle_invoice", sub)) => {
            let payment_hash = sub.get_one::<String>("payment_hash").unwrap();
            let payment_preimage = sub.get_one::<String>("payment_preimage").unwrap();
            let params = json!({
                "payment_hash": payment_hash,
                "payment_preimage": payment_preimage,
            });
            client.call_with_params("settle_invoice", params).await
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown invoice subcommand. Use --help")),
    }
}
