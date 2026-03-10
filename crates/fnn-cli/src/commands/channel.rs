use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use fiber_json_types::{
    AbandonChannelParams, AcceptChannelParams, Hash256, ListChannelsParams, ListChannelsResult,
    OpenChannelParams, OpenChannelResult, Pubkey, ShutdownChannelParams, UpdateChannelParams,
};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("channel")
        .about("Manage payment channels")
        .subcommand(
            Command::new("open_channel")
                .about("Open a channel with a peer")
                .arg(
                    Arg::new("pubkey")
                        .long("pubkey")
                        .required(true)
                        .help("The public key of the peer to open a channel with"),
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
                    Arg::new("pubkey")
                        .long("pubkey")
                        .help("Filter by public key"),
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
        Some(("open_channel", sub)) => {
            let pubkey: Pubkey = sub
                .get_one::<String>("pubkey")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid pubkey: {}", e))?;
            let funding_amount: u128 = sub
                .get_one::<String>("funding_amount")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid funding_amount"))?;

            let funding_udt_type_script = sub
                .get_one::<String>("funding_udt_type_script")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid funding_udt_type_script JSON: {}", e))?;
            let shutdown_script = sub
                .get_one::<String>("shutdown_script")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid shutdown_script JSON: {}", e))?;
            let commitment_delay_epoch = sub
                .get_one::<String>("commitment_delay_epoch")
                .map(|s| serde_json::from_str(&format!("\"{}\"", s)))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid commitment_delay_epoch: {}", e))?;

            let params = OpenChannelParams {
                pubkey,
                funding_amount,
                public: parse_optional_bool(sub, "public", true),
                one_way: parse_optional_bool(sub, "one_way", false),
                funding_udt_type_script,
                shutdown_script,
                commitment_delay_epoch,
                commitment_fee_rate: parse_optional_u64(sub, "commitment_fee_rate")?,
                funding_fee_rate: parse_optional_u64(sub, "funding_fee_rate")?,
                tlc_expiry_delta: parse_optional_u64(sub, "tlc_expiry_delta")?,
                tlc_min_value: parse_optional_u128(sub, "tlc_min_value")?,
                tlc_fee_proportional_millionths: parse_optional_u128(
                    sub,
                    "tlc_fee_proportional_millionths",
                )?,
                max_tlc_value_in_flight: parse_optional_u128(sub, "max_tlc_value_in_flight")?,
                max_tlc_number_in_flight: parse_optional_u64(sub, "max_tlc_number_in_flight")?,
            };
            let result: OpenChannelResult = client.call_typed("open_channel", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("accept_channel", sub)) => {
            let temporary_channel_id: Hash256 = sub
                .get_one::<String>("temporary_channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid temporary_channel_id: {}", e))?;
            let funding_amount: u128 = sub
                .get_one::<String>("funding_amount")
                .unwrap()
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid funding_amount"))?;

            let shutdown_script = sub
                .get_one::<String>("shutdown_script")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid shutdown_script JSON: {}", e))?;

            let params = AcceptChannelParams {
                temporary_channel_id,
                funding_amount,
                shutdown_script,
                max_tlc_value_in_flight: parse_optional_u128(sub, "max_tlc_value_in_flight")?,
                max_tlc_number_in_flight: parse_optional_u64(sub, "max_tlc_number_in_flight")?,
                tlc_min_value: parse_optional_u128(sub, "tlc_min_value")?,
                tlc_fee_proportional_millionths: parse_optional_u128(
                    sub,
                    "tlc_fee_proportional_millionths",
                )?,
                tlc_expiry_delta: parse_optional_u64(sub, "tlc_expiry_delta")?,
            };
            let result: Value = client.call_typed("accept_channel", &params).await?;
            Ok(result)
        }
        Some(("abandon_channel", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let params = AbandonChannelParams { channel_id };
            let result: Value = client.call_typed("abandon_channel", &params).await?;
            Ok(result)
        }
        Some(("list_channels", sub)) => {
            let pubkey = sub
                .get_one::<String>("pubkey")
                .map(|s| s.parse::<Pubkey>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid pubkey: {}", e))?;
            let include_closed = parse_optional_bool(sub, "include_closed", false);
            let only_pending = parse_optional_bool(sub, "only_pending", false);

            let params = ListChannelsParams {
                pubkey,
                include_closed,
                only_pending,
            };
            let result: ListChannelsResult = client.call_typed("list_channels", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        Some(("shutdown_channel", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let close_script = sub
                .get_one::<String>("close_script")
                .map(|s| serde_json::from_str(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid close_script JSON: {}", e))?;
            let fee_rate = parse_optional_u64(sub, "fee_rate")?;
            let force = parse_optional_bool(sub, "force", false);

            let params = ShutdownChannelParams {
                channel_id,
                close_script,
                fee_rate,
                force,
            };
            let result: Value = client.call_typed("shutdown_channel", &params).await?;
            Ok(result)
        }
        Some(("update_channel", sub)) => {
            let channel_id: Hash256 = sub
                .get_one::<String>("channel_id")
                .unwrap()
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid channel_id: {}", e))?;
            let enabled = parse_optional_bool(sub, "enabled", true);
            let tlc_expiry_delta = parse_optional_u64(sub, "tlc_expiry_delta")?;
            let tlc_minimum_value = parse_optional_u128(sub, "tlc_minimum_value")?;
            let tlc_fee_proportional_millionths =
                parse_optional_u128(sub, "tlc_fee_proportional_millionths")?;

            let params = UpdateChannelParams {
                channel_id,
                enabled,
                tlc_expiry_delta,
                tlc_minimum_value,
                tlc_fee_proportional_millionths,
            };
            let result: Value = client.call_typed("update_channel", &params).await?;
            Ok(result)
        }
        None => {
            command().print_help()?;
            println!();
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("Unknown channel subcommand. Use --help")),
    }
}
