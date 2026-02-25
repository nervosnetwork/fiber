use super::utils::{to_hex_u128, to_hex_u64};
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{json, Value};

pub fn command() -> Command {
    Command::new("payment")
        .about("Manage payments")
        .subcommand(
            Command::new("send_payment")
                .about("Send a payment to a peer")
                .arg(
                    Arg::new("target_pubkey")
                        .long("target-pubkey")
                        .help("The public key of the payment target"),
                )
                .arg(
                    Arg::new("amount")
                        .long("amount")
                        .help("The amount of the payment (in Shannons)"),
                )
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .help("The hash to use within the payment's HTLC"),
                )
                .arg(
                    Arg::new("final_tlc_expiry_delta")
                        .long("final-tlc-expiry-delta")
                        .help("The TLC expiry delta for the final hop, in milliseconds"),
                )
                .arg(
                    Arg::new("tlc_expiry_limit")
                        .long("tlc-expiry-limit")
                        .help("The TLC expiry limit for the whole payment, in milliseconds"),
                )
                .arg(
                    Arg::new("invoice")
                        .long("invoice")
                        .help("The encoded invoice to send to the recipient"),
                )
                .arg(
                    Arg::new("timeout")
                        .long("timeout")
                        .help("The payment timeout in seconds"),
                )
                .arg(
                    Arg::new("max_fee_amount")
                        .long("max-fee-amount")
                        .help("The maximum fee amount in Shannons"),
                )
                .arg(
                    Arg::new("max_fee_rate")
                        .long("max-fee-rate")
                        .help("The maximum fee rate per thousand"),
                )
                .arg(
                    Arg::new("max_parts")
                        .long("max-parts")
                        .help("Max parts for multi-part payments"),
                )
                .arg(
                    Arg::new("keysend")
                        .long("keysend")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Keysend payment"),
                )
                .arg(
                    Arg::new("udt_type_script")
                        .long("udt-type-script")
                        .help("UDT type script for the payment (JSON)"),
                )
                .arg(
                    Arg::new("allow_self_payment")
                        .long("allow-self-payment")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Allow self payment"),
                )
                .arg(
                    Arg::new("dry_run")
                        .long("dry-run")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Dry run for payment validation"),
                )
                .arg(
                    Arg::new("trampoline_hops")
                        .long("trampoline-hops")
                        .help("Explicit trampoline hops as JSON array of pubkeys, e.g. [\"pubkey1\",\"pubkey2\"]"),
                )
                .arg(
                    Arg::new("custom_records")
                        .long("custom-records")
                        .help("Custom records as JSON object, e.g. {\"0x1\":\"0x01020304\",\"0x2\":\"0x05060708\"}"),
                )
                .arg(
                    Arg::new("hop_hints")
                        .long("hop-hints")
                        .help("Route hints as JSON array: [{\"pubkey\":\"...\",\"channel_outpoint\":\"...\",\"fee_rate\":\"0x..\",\"tlc_expiry_delta\":\"0x..\"}]"),
                ),
        )
        .subcommand(
            Command::new("get_payment")
                .about("Retrieve a payment by payment hash")
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .required(true)
                        .help("The payment hash of the payment to retrieve"),
                ),
        )
        .subcommand(
            Command::new("list_payments")
                .about("List all payments")
                .arg(
                    Arg::new("status")
                        .long("status")
                        .help("Filter by status: Created, Inflight, Success, or Failed"),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .help("Maximum number of payments to return (default 15)"),
                )
                .arg(
                    Arg::new("after")
                        .long("after")
                        .help("Pagination cursor: payment hash to start after"),
                ),
        )
        .subcommand(
            Command::new("build_router")
                .about("Build a payment router with specified hops")
                .arg(
                    Arg::new("amount")
                        .long("amount")
                        .help("The amount of the payment (in Shannons)"),
                )
                .arg(
                    Arg::new("udt_type_script")
                        .long("udt-type-script")
                        .help("UDT type script for the payment router (JSON)"),
                )
                .arg(
                    Arg::new("hops_info")
                        .long("hops-info")
                        .required(true)
                        .help("A list of hops as JSON array: [{\"pubkey\":\"...\",\"channel_outpoint\":\"...\"}]"),
                )
                .arg(
                    Arg::new("final_tlc_expiry_delta")
                        .long("final-tlc-expiry-delta")
                        .help("The TLC expiry delta for the final hop, in milliseconds"),
                ),
        )
        .subcommand(
            Command::new("send_payment_with_router")
                .about("Send a payment with a manually specified router")
                .arg(
                    Arg::new("payment_hash")
                        .long("payment-hash")
                        .help("The hash to use within the payment's HTLC"),
                )
                .arg(
                    Arg::new("router")
                        .long("router")
                        .required(true)
                        .help("The router hops as JSON array"),
                )
                .arg(
                    Arg::new("invoice")
                        .long("invoice")
                        .help("The encoded invoice to send to the recipient"),
                )
                .arg(
                    Arg::new("keysend")
                        .long("keysend")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Keysend payment"),
                )
                .arg(
                    Arg::new("udt_type_script")
                        .long("udt-type-script")
                        .help("UDT type script for the payment (JSON)"),
                )
                .arg(
                    Arg::new("custom_records")
                        .long("custom-records")
                        .help("Custom records as JSON object, e.g. {\"0x1\":\"0x01020304\",\"0x2\":\"0x05060708\"}"),
                )
                .arg(
                    Arg::new("dry_run")
                        .long("dry-run")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Dry run for payment validation"),
                ),
        )
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("send_payment", sub)) => {
            let mut params = json!({});

            if let Some(v) = sub.get_one::<String>("target_pubkey") {
                params["target_pubkey"] = json!(v);
            }
            if let Some(v) = sub.get_one::<String>("amount") {
                let val: u128 = v.parse().map_err(|_| anyhow::anyhow!("Invalid amount"))?;
                params["amount"] = json!(to_hex_u128(val));
            }
            if let Some(v) = sub.get_one::<String>("payment_hash") {
                params["payment_hash"] = json!(v);
            }
            if let Some(v) = sub.get_one::<String>("final_tlc_expiry_delta") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid final_tlc_expiry_delta"))?;
                params["final_tlc_expiry_delta"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("tlc_expiry_limit") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid tlc_expiry_limit"))?;
                params["tlc_expiry_limit"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("invoice") {
                params["invoice"] = json!(v);
            }
            if let Some(v) = sub.get_one::<String>("timeout") {
                let val: u64 = v.parse().map_err(|_| anyhow::anyhow!("Invalid timeout"))?;
                params["timeout"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("max_fee_amount") {
                let val: u128 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid max_fee_amount"))?;
                params["max_fee_amount"] = json!(to_hex_u128(val));
            }
            if let Some(v) = sub.get_one::<String>("max_fee_rate") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid max_fee_rate"))?;
                params["max_fee_rate"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("max_parts") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid max_parts"))?;
                params["max_parts"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("keysend") {
                params["keysend"] = json!(v.parse::<bool>().unwrap_or(true));
            }
            if let Some(v) = sub.get_one::<String>("udt_type_script") {
                params["udt_type_script"] = serde_json::from_str(v)?;
            }
            if let Some(v) = sub.get_one::<String>("allow_self_payment") {
                params["allow_self_payment"] = json!(v.parse::<bool>().unwrap_or(false));
            }
            if let Some(v) = sub.get_one::<String>("dry_run") {
                params["dry_run"] = json!(v.parse::<bool>().unwrap_or(false));
            }
            if let Some(v) = sub.get_one::<String>("trampoline_hops") {
                params["trampoline_hops"] = serde_json::from_str(v)
                    .map_err(|e| anyhow::anyhow!("Invalid trampoline_hops JSON: {}", e))?;
            }
            if let Some(v) = sub.get_one::<String>("custom_records") {
                params["custom_records"] = serde_json::from_str(v)
                    .map_err(|e| anyhow::anyhow!("Invalid custom_records JSON: {}", e))?;
            }
            if let Some(v) = sub.get_one::<String>("hop_hints") {
                params["hop_hints"] = serde_json::from_str(v)
                    .map_err(|e| anyhow::anyhow!("Invalid hop_hints JSON: {}", e))?;
            }

            client.call_with_params("send_payment", params).await
        }
        Some(("get_payment", sub)) => {
            let payment_hash = sub.get_one::<String>("payment_hash").unwrap();
            let params = json!({
                "payment_hash": payment_hash,
            });
            client.call_with_params("get_payment", params).await
        }
        Some(("list_payments", sub)) => {
            let mut params = json!({});
            if let Some(v) = sub.get_one::<String>("status") {
                params["status"] = json!(v);
            }
            if let Some(v) = sub.get_one::<String>("limit") {
                let val: u64 = v.parse().map_err(|_| anyhow::anyhow!("Invalid limit"))?;
                params["limit"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("after") {
                params["after"] = json!(v);
            }
            client.call_with_params("list_payments", params).await
        }
        Some(("build_router", sub)) => {
            let mut params = json!({});

            if let Some(v) = sub.get_one::<String>("amount") {
                let val: u128 = v.parse().map_err(|_| anyhow::anyhow!("Invalid amount"))?;
                params["amount"] = json!(to_hex_u128(val));
            }
            if let Some(v) = sub.get_one::<String>("udt_type_script") {
                params["udt_type_script"] = serde_json::from_str(v)?;
            }

            let hops_info: Value =
                serde_json::from_str(sub.get_one::<String>("hops_info").unwrap())?;
            params["hops_info"] = hops_info;

            if let Some(v) = sub.get_one::<String>("final_tlc_expiry_delta") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid final_tlc_expiry_delta"))?;
                params["final_tlc_expiry_delta"] = json!(to_hex_u64(val));
            }

            client.call_with_params("build_router", params).await
        }
        Some(("send_payment_with_router", sub)) => {
            let mut params = json!({});

            if let Some(v) = sub.get_one::<String>("payment_hash") {
                params["payment_hash"] = json!(v);
            }

            let router: Value = serde_json::from_str(sub.get_one::<String>("router").unwrap())?;
            params["router"] = router;

            if let Some(v) = sub.get_one::<String>("invoice") {
                params["invoice"] = json!(v);
            }
            if let Some(v) = sub.get_one::<String>("keysend") {
                params["keysend"] = json!(v.parse::<bool>().unwrap_or(true));
            }
            if let Some(v) = sub.get_one::<String>("udt_type_script") {
                params["udt_type_script"] = serde_json::from_str(v)?;
            }
            if let Some(v) = sub.get_one::<String>("custom_records") {
                params["custom_records"] = serde_json::from_str(v)
                    .map_err(|e| anyhow::anyhow!("Invalid custom_records JSON: {}", e))?;
            }
            if let Some(v) = sub.get_one::<String>("dry_run") {
                params["dry_run"] = json!(v.parse::<bool>().unwrap_or(false));
            }

            client
                .call_with_params("send_payment_with_router", params)
                .await
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown payment subcommand. Use --help")),
    }
}
