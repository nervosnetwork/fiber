use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use fiber_json_types::{
    BuildRouterParams, GetPaymentCommandParams, GetPaymentCommandResult, Hash256,
    ListPaymentsParams, ListPaymentsResult, Pubkey, SendPaymentCommandParams,
    SendPaymentWithRouterParams,
};
use serde_json::Value;

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

fn parse_optional_u64(sub: &ArgMatches, name: &str) -> Result<Option<u64>> {
    sub.get_one::<String>(name)
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| anyhow::anyhow!("Invalid {}", name))
        })
        .transpose()
}

fn parse_optional_u128(sub: &ArgMatches, name: &str) -> Result<Option<u128>> {
    sub.get_one::<String>(name)
        .map(|v| {
            v.parse::<u128>()
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
        Some(("send_payment", sub)) => {
            let target_pubkey = sub
                .get_one::<String>("target_pubkey")
                .map(|s| s.parse::<Pubkey>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid target_pubkey: {}", e))?;
            let amount = parse_optional_u128(sub, "amount")?;
            let payment_hash = sub
                .get_one::<String>("payment_hash")
                .map(|s| s.parse::<Hash256>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;
            let final_tlc_expiry_delta = parse_optional_u64(sub, "final_tlc_expiry_delta")?;
            let tlc_expiry_limit = parse_optional_u64(sub, "tlc_expiry_limit")?;
            let invoice = sub.get_one::<String>("invoice").cloned();
            let timeout = parse_optional_u64(sub, "timeout")?;
            let max_fee_amount = parse_optional_u128(sub, "max_fee_amount")?;
            let max_fee_rate = parse_optional_u64(sub, "max_fee_rate")?;
            let max_parts = parse_optional_u64(sub, "max_parts")?;
            let keysend = parse_optional_bool(sub, "keysend", true);
            let udt_type_script = sub
                .get_one::<String>("udt_type_script")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid udt_type_script JSON: {}", e))?;
            let allow_self_payment = parse_optional_bool(sub, "allow_self_payment", false);
            let dry_run = parse_optional_bool(sub, "dry_run", false);
            let trampoline_hops = sub
                .get_one::<String>("trampoline_hops")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid trampoline_hops JSON: {}", e))?;
            let custom_records = sub
                .get_one::<String>("custom_records")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid custom_records JSON: {}", e))?;
            let hop_hints = sub
                .get_one::<String>("hop_hints")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid hop_hints JSON: {}", e))?;

            let params = SendPaymentCommandParams {
                target_pubkey,
                amount,
                payment_hash,
                final_tlc_expiry_delta,
                tlc_expiry_limit,
                invoice,
                timeout,
                max_fee_amount,
                max_fee_rate,
                max_parts,
                keysend,
                udt_type_script,
                allow_self_payment,
                dry_run,
                trampoline_hops,
                custom_records,
                hop_hints,
            };
            let result: Value = client.call_typed("send_payment", &params).await?;
            Ok(result)
        }
        Some(("get_payment", sub)) => {
            let payment_hash: Hash256 = sub
                .get_one::<String>("payment_hash")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;
            let params = GetPaymentCommandParams { payment_hash };
            let result: GetPaymentCommandResult = client.call_typed("get_payment", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("list_payments", sub)) => {
            let status = sub
                .get_one::<String>("status")
                .map(|s| serde_json::from_value(Value::String(s.clone())))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid status: {}", e))?;
            let limit = parse_optional_u64(sub, "limit")?;
            let after = sub
                .get_one::<String>("after")
                .map(|s| s.parse::<Hash256>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid after: {}", e))?;

            let params = ListPaymentsParams {
                status,
                limit,
                after,
            };
            let result: ListPaymentsResult = client.call_typed("list_payments", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("build_router", sub)) => {
            let amount = parse_optional_u128(sub, "amount")?;
            let udt_type_script = sub
                .get_one::<String>("udt_type_script")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid udt_type_script JSON: {}", e))?;
            let hops_info = serde_json::from_str(sub.get_one::<String>("hops_info").unwrap())
                .map_err(|e| anyhow::anyhow!("Invalid hops_info JSON: {}", e))?;
            let final_tlc_expiry_delta = parse_optional_u64(sub, "final_tlc_expiry_delta")?;

            let params = BuildRouterParams {
                amount,
                udt_type_script,
                hops_info,
                final_tlc_expiry_delta,
            };
            let result: Value = client.call_typed("build_router", &params).await?;
            Ok(result)
        }
        Some(("send_payment_with_router", sub)) => {
            let payment_hash = sub
                .get_one::<String>("payment_hash")
                .map(|s| s.parse::<Hash256>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid payment_hash: {}", e))?;
            let router = serde_json::from_str(sub.get_one::<String>("router").unwrap())
                .map_err(|e| anyhow::anyhow!("Invalid router JSON: {}", e))?;
            let invoice = sub.get_one::<String>("invoice").cloned();
            let keysend = parse_optional_bool(sub, "keysend", true);
            let udt_type_script = sub
                .get_one::<String>("udt_type_script")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid udt_type_script JSON: {}", e))?;
            let custom_records = sub
                .get_one::<String>("custom_records")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid custom_records JSON: {}", e))?;
            let dry_run = parse_optional_bool(sub, "dry_run", false);

            let params = SendPaymentWithRouterParams {
                payment_hash,
                router,
                invoice,
                keysend,
                udt_type_script,
                custom_records,
                dry_run,
            };
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
