use crate::cli_generated::CliArgs;
use crate::rpc_client::RpcClient;
use anyhow::Result;
use clap::{ArgMatches, Command};
use fiber_json_types::{PprofParams, PprofResult};
use serde_json::Value;

pub fn command() -> Command {
    Command::new("prof")
        .about("Profiling commands (requires pprof feature)")
        .subcommand(PprofParams::augment_command(
            Command::new("pprof").about("Collect a CPU profile and write a flamegraph SVG to disk"),
        ))
}

pub async fn execute(client: &RpcClient, matches: &ArgMatches) -> Result<Value> {
    match matches.subcommand() {
        Some(("pprof", sub)) => {
            let params = PprofParams::from_arg_matches(sub)?;
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
