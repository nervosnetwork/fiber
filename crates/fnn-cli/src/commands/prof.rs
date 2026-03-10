use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use fiber_json_types::{PprofParams, PprofResult};
use serde_json::Value;

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
            let duration_secs = sub
                .get_one::<String>("duration_secs")
                .map(|v| {
                    v.parse::<u64>()
                        .map_err(|_| anyhow::anyhow!("Invalid duration_secs"))
                })
                .transpose()?;

            let params = PprofParams { duration_secs };
            let result: PprofResult = client.call_typed("pprof", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        None => {
            // Default to pprof with no params
            let params = PprofParams {
                duration_secs: None,
            };
            let result: PprofResult = client.call_typed("pprof", &params).await?;
            serde_json::to_value(result).map_err(Into::into)
        }
        _ => Err(anyhow::anyhow!("Unknown prof subcommand. Use --help")),
    }
}
