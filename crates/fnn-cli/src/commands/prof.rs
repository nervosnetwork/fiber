use super::utils::to_hex_u64;
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{json, Value};

pub fn command() -> Command {
    Command::new("prof")
        .about("Profiling commands (requires pprof feature)")
        .subcommand(
            Command::new("pprof")
                .about("Collect a CPU profile and write a flamegraph SVG to disk")
                .arg(
                    Arg::new("duration_secs")
                        .long("duration-secs")
                        .help("Duration to profile in seconds (default: 10)"),
                ),
        )
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("pprof", sub)) => {
            let mut params = json!({});

            if let Some(v) = sub.get_one::<String>("duration_secs") {
                let val: u64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid duration_secs"))?;
                params["duration_secs"] = json!(to_hex_u64(val));
            }

            client.call_with_params("pprof", params).await
        }
        None => {
            // Default to pprof with no params
            client.call_with_params("pprof", json!({})).await
        }
        _ => Err(anyhow::anyhow!("Unknown prof subcommand. Use --help")),
    }
}
