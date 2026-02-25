use super::utils::{to_hex_u128, to_hex_u64};
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{json, Value};

pub fn command() -> Command {
    Command::new("channel")
        .about("Manage payment channels")
        .subcommand(
            Command::new("open_channel")
                .about("Open a channel with a peer")
                .arg(
                    Arg::new("peer_id")
                        .long("peer-id")
                        .required(true)
                        .help("The peer ID to open a channel with"),
                )
                .arg(
                    Arg::new("funding_amount")
                        .long("funding-amount")
                        .required(true)
                        .help("The amount of CKB or UDT to fund the channel with (in Shannons)"),
                )
                .arg(
                    Arg::new("public")
                        .long("public")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Whether this is a public channel (default: true)"),
                )
                .arg(
                    Arg::new("one_way")
                        .long("one-way")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Whether this is a one-way channel (default: false)"),
                )
                .arg(
                    Arg::new("funding_udt_type_script")
                        .long("funding-udt-type-script")
                        .help("The UDT type script as JSON"),
                )
                .arg(
                    Arg::new("shutdown_script")
                        .long("shutdown-script")
                        .help("The script used to receive the channel balance (JSON)"),
                )
                .arg(
                    Arg::new("commitment_delay_epoch")
                        .long("commitment-delay-epoch")
                        .help("The delay time for the commitment transaction (hex u64)"),
                )
                .arg(
                    Arg::new("commitment_fee_rate")
                        .long("commitment-fee-rate")
                        .help("The fee rate for the commitment transaction"),
                )
                .arg(
                    Arg::new("funding_fee_rate")
                        .long("funding-fee-rate")
                        .help("The fee rate for the funding transaction"),
                )
                .arg(
                    Arg::new("tlc_expiry_delta")
                        .long("tlc-expiry-delta")
                        .help("The expiry delta to forward a TLC, in milliseconds"),
                )
                .arg(
                    Arg::new("tlc_min_value")
                        .long("tlc-min-value")
                        .help("The minimum value for a TLC"),
                )
                .arg(
                    Arg::new("tlc_fee_proportional_millionths")
                        .long("tlc-fee-proportional-millionths")
                        .help("The fee proportional millionths for a TLC"),
                )
                .arg(
                    Arg::new("max_tlc_value_in_flight")
                        .long("max-tlc-value-in-flight")
                        .help("The maximum value in flight for TLCs"),
                )
                .arg(
                    Arg::new("max_tlc_number_in_flight")
                        .long("max-tlc-number-in-flight")
                        .help("The maximum number of TLCs that can be accepted"),
                ),
        )
        .subcommand(
            Command::new("accept_channel")
                .about("Accept a channel opening request")
                .arg(
                    Arg::new("temporary_channel_id")
                        .long("temporary-channel-id")
                        .required(true)
                        .help("The temporary channel ID of the channel to accept"),
                )
                .arg(
                    Arg::new("funding_amount")
                        .long("funding-amount")
                        .required(true)
                        .help("The amount of CKB or UDT to fund the channel with"),
                )
                .arg(
                    Arg::new("shutdown_script")
                        .long("shutdown-script")
                        .help("The script used to receive the channel balance (JSON)"),
                )
                .arg(
                    Arg::new("max_tlc_value_in_flight")
                        .long("max-tlc-value-in-flight")
                        .help("The max TLC sum value in flight"),
                )
                .arg(
                    Arg::new("max_tlc_number_in_flight")
                        .long("max-tlc-number-in-flight")
                        .help("The max TLC number in flight"),
                )
                .arg(
                    Arg::new("tlc_min_value")
                        .long("tlc-min-value")
                        .help("The minimum value for a TLC"),
                )
                .arg(
                    Arg::new("tlc_fee_proportional_millionths")
                        .long("tlc-fee-proportional-millionths")
                        .help("The fee proportional millionths for a TLC"),
                )
                .arg(
                    Arg::new("tlc_expiry_delta")
                        .long("tlc-expiry-delta")
                        .help("The expiry delta to forward a TLC, in milliseconds"),
                ),
        )
        .subcommand(
            Command::new("abandon_channel")
                .about("Abandon a channel")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID of the channel to abandon"),
                ),
        )
        .subcommand(
            Command::new("list_channels")
                .about("List all channels")
                .arg(
                    Arg::new("peer_id")
                        .long("peer-id")
                        .help("Filter by peer ID"),
                )
                .arg(
                    Arg::new("include_closed")
                        .long("include-closed")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Include closed channels (default: false)"),
                )
                .arg(
                    Arg::new("only_pending")
                        .long("only-pending")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Only show pending channels (default: false)"),
                ),
        )
        .subcommand(
            Command::new("shutdown_channel")
                .about("Shut down a channel")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID of the channel to shut down"),
                )
                .arg(
                    Arg::new("close_script")
                        .long("close-script")
                        .help("The script used to receive the channel balance (JSON)"),
                )
                .arg(
                    Arg::new("fee_rate")
                        .long("fee-rate")
                        .help("The fee rate for the closing transaction"),
                )
                .arg(
                    Arg::new("force")
                        .long("force")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Force close the channel (default: false)"),
                ),
        )
        .subcommand(
            Command::new("update_channel")
                .about("Update a channel's parameters")
                .arg(
                    Arg::new("channel_id")
                        .long("channel-id")
                        .required(true)
                        .help("The channel ID of the channel to update"),
                )
                .arg(
                    Arg::new("enabled")
                        .long("enabled")
                        .num_args(0..=1)
                        .default_missing_value("true")
                        .help("Whether the channel is enabled"),
                )
                .arg(
                    Arg::new("tlc_expiry_delta")
                        .long("tlc-expiry-delta")
                        .help("The expiry delta for the TLC locktime"),
                )
                .arg(
                    Arg::new("tlc_minimum_value")
                        .long("tlc-minimum-value")
                        .help("The minimum value for a TLC"),
                )
                .arg(
                    Arg::new("tlc_fee_proportional_millionths")
                        .long("tlc-fee-proportional-millionths")
                        .help("The fee proportional millionths for a TLC"),
                ),
        )
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("open_channel", sub)) => {
            let peer_id = sub.get_one::<String>("peer_id").unwrap();
            let funding_amount: u128 = sub
                .get_one::<String>("funding_amount")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid funding_amount"))?;

            let mut params = json!({
                "peer_id": peer_id,
                "funding_amount": to_hex_u128(funding_amount),
            });

            if let Some(public) = sub.get_one::<String>("public") {
                params["public"] = json!(public.parse::<bool>().unwrap_or(true));
            }
            if let Some(one_way) = sub.get_one::<String>("one_way") {
                params["one_way"] = json!(one_way.parse::<bool>().unwrap_or(false));
            }
            if let Some(script) = sub.get_one::<String>("funding_udt_type_script") {
                params["funding_udt_type_script"] = serde_json::from_str(script)?;
            }
            if let Some(script) = sub.get_one::<String>("shutdown_script") {
                params["shutdown_script"] = serde_json::from_str(script)?;
            }
            if let Some(v) = sub.get_one::<String>("commitment_delay_epoch") {
                params["commitment_delay_epoch"] = json!(v);
            }
            if let Some(v) = sub.get_one::<String>("commitment_fee_rate") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid commitment_fee_rate"))?;
                params["commitment_fee_rate"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("funding_fee_rate") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid funding_fee_rate"))?;
                params["funding_fee_rate"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("tlc_expiry_delta") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid tlc_expiry_delta"))?;
                params["tlc_expiry_delta"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("tlc_min_value") {
                let val: u128 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid tlc_min_value"))?;
                params["tlc_min_value"] = json!(to_hex_u128(val));
            }
            if let Some(v) = sub.get_one::<String>("tlc_fee_proportional_millionths") {
                let val: u128 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid tlc_fee_proportional_millionths"))?;
                params["tlc_fee_proportional_millionths"] = json!(to_hex_u128(val));
            }
            if let Some(v) = sub.get_one::<String>("max_tlc_value_in_flight") {
                let val: u128 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid max_tlc_value_in_flight"))?;
                params["max_tlc_value_in_flight"] = json!(to_hex_u128(val));
            }
            if let Some(v) = sub.get_one::<String>("max_tlc_number_in_flight") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid max_tlc_number_in_flight"))?;
                params["max_tlc_number_in_flight"] = json!(to_hex_u64(val));
            }

            client.call_with_params("open_channel", params).await
        }
        Some(("accept_channel", sub)) => {
            let temp_id = sub.get_one::<String>("temporary_channel_id").unwrap();
            let funding_amount: u128 = sub
                .get_one::<String>("funding_amount")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid funding_amount"))?;

            let mut params = json!({
                "temporary_channel_id": temp_id,
                "funding_amount": to_hex_u128(funding_amount),
            });

            if let Some(script) = sub.get_one::<String>("shutdown_script") {
                params["shutdown_script"] = serde_json::from_str(script)?;
            }
            if let Some(v) = sub.get_one::<String>("max_tlc_value_in_flight") {
                let val: u128 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid max_tlc_value_in_flight"))?;
                params["max_tlc_value_in_flight"] = json!(to_hex_u128(val));
            }
            if let Some(v) = sub.get_one::<String>("max_tlc_number_in_flight") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid max_tlc_number_in_flight"))?;
                params["max_tlc_number_in_flight"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("tlc_min_value") {
                let val: u128 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid tlc_min_value"))?;
                params["tlc_min_value"] = json!(to_hex_u128(val));
            }
            if let Some(v) = sub.get_one::<String>("tlc_fee_proportional_millionths") {
                let val: u128 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid tlc_fee_proportional_millionths"))?;
                params["tlc_fee_proportional_millionths"] = json!(to_hex_u128(val));
            }
            if let Some(v) = sub.get_one::<String>("tlc_expiry_delta") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid tlc_expiry_delta"))?;
                params["tlc_expiry_delta"] = json!(to_hex_u64(val));
            }

            client.call_with_params("accept_channel", params).await
        }
        Some(("abandon_channel", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let params = json!({
                "channel_id": channel_id,
            });
            client.call_with_params("abandon_channel", params).await
        }
        Some(("list_channels", sub)) => {
            let mut params = json!({});

            if let Some(peer_id) = sub.get_one::<String>("peer_id") {
                params["peer_id"] = json!(peer_id);
            }
            if let Some(v) = sub.get_one::<String>("include_closed") {
                params["include_closed"] = json!(v.parse::<bool>().unwrap_or(false));
            }
            if let Some(v) = sub.get_one::<String>("only_pending") {
                params["only_pending"] = json!(v.parse::<bool>().unwrap_or(false));
            }

            client.call_with_params("list_channels", params).await
        }
        Some(("shutdown_channel", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let mut params = json!({
                "channel_id": channel_id,
            });

            if let Some(script) = sub.get_one::<String>("close_script") {
                params["close_script"] = serde_json::from_str(script)?;
            }
            if let Some(v) = sub.get_one::<String>("fee_rate") {
                let val: u64 = v.parse().map_err(|_| anyhow::anyhow!("Invalid fee_rate"))?;
                params["fee_rate"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("force") {
                params["force"] = json!(v.parse::<bool>().unwrap_or(false));
            }

            client.call_with_params("shutdown_channel", params).await
        }
        Some(("update_channel", sub)) => {
            let channel_id = sub.get_one::<String>("channel_id").unwrap();
            let mut params = json!({
                "channel_id": channel_id,
            });

            if let Some(v) = sub.get_one::<String>("enabled") {
                params["enabled"] = json!(v.parse::<bool>().unwrap_or(true));
            }
            if let Some(v) = sub.get_one::<String>("tlc_expiry_delta") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid tlc_expiry_delta"))?;
                params["tlc_expiry_delta"] = json!(to_hex_u64(val));
            }
            if let Some(v) = sub.get_one::<String>("tlc_minimum_value") {
                let val: u128 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid tlc_minimum_value"))?;
                params["tlc_minimum_value"] = json!(to_hex_u128(val));
            }
            if let Some(v) = sub.get_one::<String>("tlc_fee_proportional_millionths") {
                let val: u128 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid tlc_fee_proportional_millionths"))?;
                params["tlc_fee_proportional_millionths"] = json!(to_hex_u128(val));
            }

            client.call_with_params("update_channel", params).await
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown channel subcommand. Use --help")),
    }
}
