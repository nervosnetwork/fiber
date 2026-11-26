//! `fnn drop-v1-channel`: offline pre-migration archive of selected V1 channels.
//!
//! This removes the channel from the active store after copying its records under
//! the archived-V1 prefix. It does not close the funding transaction and it does
//! not recover funds. Run it while the node is stopped, then start the node so the
//! normal migration can proceed.

use std::path::PathBuf;

use fnn::fiber_types::Hash256;
use fnn::store::{archive_v1_channels, V1ChannelArchiveSummary};

fn usage() -> &'static str {
    "Usage: fnn drop-v1-channel --store-path <path> --channel-id <hex> [--channel-id <hex> ...] [--dry-run] [--yes]\n\
     \n\
     --store-path   Fiber store directory (the node's <base>/fiber/store).\n\
     --channel-id   V1 channel id to archive. Repeat for multiple channels.\n\
     --dry-run      Validate and list what would be archived without writing.\n\
     --yes          Skip the interactive confirmation.\n\
     \n\
     WARNING: this does not close the funding transaction. If the funding output is\n\
     already on chain, the funds can remain locked or be lost. Back up the store first."
}

fn parse_hex_id(value: &str) -> Result<Hash256, String> {
    value
        .parse::<Hash256>()
        .map_err(|error| format!("invalid channel id {value:?}: {error}"))
}

fn print_summaries(summaries: &[V1ChannelArchiveSummary]) {
    for summary in summaries {
        eprintln!("channel: {}", summary.channel_id);
        eprintln!("  state: {}", summary.state);
        if let Some(hash) = &summary.funding_tx_hash {
            eprintln!("  funding_tx: {hash}");
        }
        if let Some(outpoint) = &summary.funding_outpoint {
            eprintln!("  funding_outpoint: {outpoint}");
        }
        eprintln!(
            "  active keys to archive: {} (watchtower: {})",
            summary.archived_keys, summary.watchtower_keys
        );
    }
}

fn confirm() -> Result<(), String> {
    eprintln!(
        "\nThis archives the V1 channel records and removes them from the active store.\n\
         It does not close the funding transaction. If funding is already on chain,\n\
         the funds may be permanently lost.\n\
         Type 'yes' to continue:"
    );
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(|error| format!("failed to read confirmation: {error}"))?;
    if input.trim().eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        Err("aborted".to_owned())
    }
}

pub(crate) fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut store_path: Option<PathBuf> = None;
    let mut channel_ids = Vec::new();
    let mut dry_run = false;
    let mut yes = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--store-path" => {
                let value = args.next().ok_or("--store-path requires a value")?;
                store_path = Some(PathBuf::from(value));
            }
            "--channel-id" => {
                let value = args.next().ok_or("--channel-id requires a value")?;
                channel_ids.push(parse_hex_id(&value)?);
            }
            "--dry-run" => dry_run = true,
            "--yes" | "-y" => yes = true,
            "--help" | "-h" => {
                eprintln!("{}", usage());
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
    }

    let store_path = store_path.ok_or_else(|| format!("--store-path is required\n{}", usage()))?;
    if channel_ids.is_empty() {
        return Err(format!(
            "at least one --channel-id is required\n{}",
            usage()
        ));
    }

    let summaries = archive_v1_channels(&store_path, &channel_ids, false)?;
    print_summaries(&summaries);
    if dry_run {
        eprintln!("\nDry run: no records were changed.");
        return Ok(());
    }

    if !yes {
        confirm()?;
    }

    let applied = archive_v1_channels(&store_path, &channel_ids, true)?;
    print_summaries(&applied);
    eprintln!(
        "\nArchived {} V1 channel(s). The database version was not changed; start the node to run the normal migration.",
        applied.len()
    );
    Ok(())
}
