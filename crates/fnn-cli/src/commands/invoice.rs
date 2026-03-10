use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use fiber_json_types::{
    GetInvoiceResult, Hash256, InvoiceParams, InvoiceResult, NewInvoiceParams, ParseInvoiceParams,
    ParseInvoiceResult, SettleInvoiceParams,
};
use serde_json::Value;

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

fn parse_optional_u64(sub: &ArgMatches, name: &str) -> Result<Option<u64>> {
    sub.get_one::<String>(name)
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| anyhow::anyhow!("Invalid {}", name))
        })
        .transpose()
}

fn parse_optional_bool(sub: &ArgMatches, name: &str, default: bool) -> Option<bool> {
    sub.get_one::<String>(name)
        .map(|v| v.parse::<bool>().unwrap_or(default))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("new_invoice", sub)) => {
            let amount: u128 = sub
                .get_one::<String>("amount")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid amount"))?;
            let currency = serde_json::from_value(Value::String(
                sub.get_one::<String>("currency").unwrap().clone(),
            ))
            .map_err(|e| anyhow::anyhow!("Invalid currency: {}", e))?;
            let description = sub.get_one::<String>("description").cloned();
            let payment_preimage = sub
                .get_one::<String>("payment_preimage")
                .map(|s| s.parse::<Hash256>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid payment_preimage: {}", e))?;
            let payment_hash = sub
                .get_one::<String>("payment_hash")
                .map(|s| s.parse::<Hash256>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;
            let expiry = parse_optional_u64(sub, "expiry")?;
            let fallback_address = sub.get_one::<String>("fallback_address").cloned();
            let final_expiry_delta = parse_optional_u64(sub, "final_expiry_delta")?;
            let udt_type_script = sub
                .get_one::<String>("udt_type_script")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid udt_type_script JSON: {}", e))?;
            let hash_algorithm = sub
                .get_one::<String>("hash_algorithm")
                .map(|s| serde_json::from_value(Value::String(s.clone())))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid hash_algorithm: {}", e))?;
            let allow_mpp = parse_optional_bool(sub, "allow_mpp", true);
            let allow_trampoline_routing =
                parse_optional_bool(sub, "allow_trampoline_routing", true);

            let params = NewInvoiceParams {
                amount,
                currency,
                description,
                payment_preimage,
                payment_hash,
                expiry,
                fallback_address,
                final_expiry_delta,
                udt_type_script,
                hash_algorithm,
                allow_mpp,
                allow_trampoline_routing,
            };
            let result: InvoiceResult = client.call_typed("new_invoice", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("parse_invoice", sub)) => {
            let invoice = sub.get_one::<String>("invoice").unwrap().clone();
            let params = ParseInvoiceParams { invoice };
            let result: ParseInvoiceResult = client.call_typed("parse_invoice", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("get_invoice", sub)) => {
            let payment_hash: Hash256 = sub
                .get_one::<String>("payment_hash")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;
            let params = InvoiceParams { payment_hash };
            let result: GetInvoiceResult = client.call_typed("get_invoice", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("cancel_invoice", sub)) => {
            let payment_hash: Hash256 = sub
                .get_one::<String>("payment_hash")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;
            let params = InvoiceParams { payment_hash };
            let result: Value = client.call_typed("cancel_invoice", &params).await?;
            Ok(result)
        }
        Some(("settle_invoice", sub)) => {
            let payment_hash: Hash256 = sub
                .get_one::<String>("payment_hash")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;
            let payment_preimage: Hash256 = sub
                .get_one::<String>("payment_preimage")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid payment_preimage: {}", e))?;
            let params = SettleInvoiceParams {
                payment_hash,
                payment_preimage,
            };
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
