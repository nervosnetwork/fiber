pub mod cch;
pub mod channel;
pub mod dev;
pub mod graph;
pub mod info;
pub mod invoice;
pub mod payment;
pub mod peer;
pub mod prof;
pub mod utils;
pub mod watchtower;

use crate::colorize;
use crate::rpc_client::RpcClient;
use anyhow::Result;
use serde_json::Value;

/// Format and print the RPC response based on the output settings.
pub fn print_result(value: &Value, raw_data: bool, output_format: &str, color: bool) {
    // Skip printing if the result is null (e.g., from help output)
    if value.is_null() {
        return;
    }

    if raw_data {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        );
        return;
    }

    match output_format {
        "json" => {
            if color {
                println!("{}", colorize::colorize_json(value));
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
                );
            }
        }
        "yaml" => {
            if color {
                println!("{}", colorize::colorize_yaml(value));
            } else {
                println!(
                    "{}",
                    serde_yaml::to_string(value).unwrap_or_else(|_| value.to_string())
                );
            }
        }
        _ => {
            if color {
                println!("{}", colorize::colorize_json(value));
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
                );
            }
        }
    }
}

/// Execute an RPC subcommand and print its result.
pub async fn execute_command(
    client: &RpcClient,
    command: &clap::ArgMatches,
    output_format: &str,
    color: bool,
) -> Result<()> {
    let raw = client.raw_data();

    match command.subcommand() {
        Some(("info", sub)) => {
            let result = info::execute(client, sub).await?;
            print_result(&result, raw, output_format, color);
        }
        Some(("peer", sub)) => {
            let result = peer::execute(client, sub).await?;
            print_result(&result, raw, output_format, color);
        }
        Some(("channel", sub)) => {
            let result = channel::execute(client, sub).await?;
            print_result(&result, raw, output_format, color);
        }
        Some(("invoice", sub)) => {
            let result = invoice::execute(client, sub).await?;
            print_result(&result, raw, output_format, color);
        }
        Some(("payment", sub)) => {
            let result = payment::execute(client, sub).await?;
            print_result(&result, raw, output_format, color);
        }
        Some(("graph", sub)) => {
            let result = graph::execute(client, sub).await?;
            print_result(&result, raw, output_format, color);
        }
        Some(("cch", sub)) => {
            let result = cch::execute(client, sub).await?;
            print_result(&result, raw, output_format, color);
        }
        Some(("dev", sub)) => {
            let result = dev::execute(client, sub).await?;
            print_result(&result, raw, output_format, color);
        }
        Some(("watchtower", sub)) => {
            let result = watchtower::execute(client, sub).await?;
            print_result(&result, raw, output_format, color);
        }
        Some(("prof", sub)) => {
            let result = prof::execute(client, sub).await?;
            print_result(&result, raw, output_format, color);
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Unknown command. Use --help for available commands."
            ));
        }
    }

    Ok(())
}
