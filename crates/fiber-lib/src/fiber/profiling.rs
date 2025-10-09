use std::path::PathBuf;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use pprof::ProfilerGuardBuilder;

const OUTPUT_DIR: &str = "profiles";

/// Collects a flamegraph for the given duration and writes it under `./profiles`.
pub async fn collect_flamegraph(duration_secs: u64) -> Result<PathBuf> {
    let duration_secs = duration_secs.max(1);

    tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let guard = ProfilerGuardBuilder::default()
            .frequency(99)
            .build()
            .context("starting profiler")?;

        std::thread::sleep(Duration::from_secs(duration_secs));

        let report = guard.report().build().context("building profiler report")?;

        std::fs::create_dir_all(OUTPUT_DIR).context("creating output directory")?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("computing timestamp")?
            .as_secs();
        let output = PathBuf::from(OUTPUT_DIR).join(format!("flamegraph-{timestamp}.svg"));

        let file = std::fs::File::create(&output).context("creating flamegraph file")?;
        report.flamegraph(file).context("writing flamegraph svg")?;

        Ok(output)
    })
    .await
    .context("profiling task panicked")?
}
