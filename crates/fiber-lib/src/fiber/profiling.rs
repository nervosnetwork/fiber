use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use once_cell::sync::OnceCell;
use tokio::sync::RwLock;

use pprof::{protos::Message, ProfilerGuard, ProfilerGuardBuilder};

const OUTPUT_DIR: &str = "profiles";
const SAMPLE_FREQUENCY: i32 = 1000;

static PROFILER: OnceCell<Arc<Profiler>> = OnceCell::new();

/// Describes the exported artifacts produced from a captured CPU profile.
#[derive(Debug, Clone)]
pub struct ProfileArtifacts {
    /// Path to the SVG flamegraph for quick visual inspection.
    pub flamegraph_svg: PathBuf,
    /// Path to the raw pprof profile (protobuf) compatible with `go tool pprof` and `perf`.
    pub raw_profile: PathBuf,
}

/// Convenience helper for ad-hoc captures.
///
/// Starts the global profiler, awaits the requested duration without blocking the
/// runtime, and exports both an SVG flamegraph and a protobuf profile under
/// `./profiles`.
pub async fn collect_flamegraph(duration_secs: u64) -> Result<PathBuf> {
    let duration_secs = duration_secs.max(1);

    global_profiler().start().await?;

    tokio::time::sleep(Duration::from_secs(duration_secs)).await;

    global_profiler().stop().await?;
    let artifacts = global_profiler().export().await?;

    Ok(artifacts.flamegraph_svg)
}

fn global_profiler() -> &'static Arc<Profiler> {
    PROFILER.get_or_init(|| Arc::new(Profiler::default()))
}

/// Tracks the lifetime of a single process-wide `ProfilerGuard`.
///
/// A guard installed once via `pprof` hooks into the OS signal-based sampler, so
/// every Tokio worker, `spawn_blocking` helper, and custom thread in the process is
/// sampled without needing per-thread instrumentation. Keeping it in a global
/// singleton ensures we never lose samples because a request handler dropped the
/// guard too early.
#[derive(Default)]
struct Profiler {
    state: RwLock<ProfilerState>,
}

#[derive(Default)]
struct ProfilerState {
    /// Active guard while profiling is in flight.
    guard: Option<ProfilerGuard<'static>>,
    /// Last completed profile, ready to be written to disk by any caller.
    last_capture: Option<CompletedProfile>,
}

#[derive(Clone)]
struct CompletedProfile {
    /// Pre-rendered flamegraph bytes so exports avoid recomputing heavy work.
    flamegraph: Arc<Vec<u8>>,
    /// Serialized protobuf profile compatible with external tooling.
    protobuf: Arc<Vec<u8>>,
    /// Wall-clock start time of the capture for easy correlation.
    started_at: SystemTime,
    /// Total runtime of the capture window.
    duration: Duration,
}

impl Profiler {
    async fn start(&self) -> Result<()> {
        {
            let state = self.state.read().await;
            if state.guard.is_some() {
                bail!("CPU profiler is already running");
            }
        }

        // Building the guard installs signal handlers and touches TLS state, so keep it off the runtime.
        let guard = tokio::task::spawn_blocking(|| {
            ProfilerGuardBuilder::default()
                .frequency(SAMPLE_FREQUENCY)
                .blocklist(&["libc", "libgcc", "pthread", "vdso"])
                .build()
                .context("starting profiler")
        })
        .await
        .context("joining profiler start task")??;

        let mut state = self.state.write().await;
        state.guard = Some(guard);
        // Discard stale exports so callers cannot read a profile from a previous run by mistake.
        state.last_capture = None;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let guard = {
            let mut state = self.state.write().await;
            state.guard.take().context("CPU profiler is not running")?
        };

        // Generating the report can take a while, so do the heavy lifting off the runtime.
        let (flamegraph_bytes, profile_bytes, started_at, duration) = tokio::task::spawn_blocking(
            move || -> Result<(Vec<u8>, Vec<u8>, SystemTime, Duration)> {
                let report = guard.report().build().context("building profiler report")?;
                let started_at = report.timing.start_time;
                let duration = report.timing.duration;

                let mut flamegraph_bytes = Vec::new();
                report
                    .flamegraph(&mut flamegraph_bytes)
                    .context("rendering flamegraph svg")?;

                let profile = report.pprof().context("building pprof profile")?;
                let mut buffer = Vec::new();
                profile
                    .write_to_vec(&mut buffer)
                    .context("serializing pprof profile")?;

                Ok((flamegraph_bytes, buffer, started_at, duration))
            },
        )
        .await
        .context("joining profiler stop task")??;

        let mut state = self.state.write().await;
        state.last_capture = Some(CompletedProfile {
            flamegraph: Arc::new(flamegraph_bytes),
            protobuf: Arc::new(profile_bytes),
            started_at,
            duration,
        });

        Ok(())
    }

    async fn export(&self) -> Result<ProfileArtifacts> {
        let capture = {
            let state = self.state.read().await;
            state
                .last_capture
                .clone()
                .context("no completed profile available; call stop_cpu_profiling() first")?
        };

        tokio::task::spawn_blocking(move || -> Result<ProfileArtifacts> {
            std::fs::create_dir_all(OUTPUT_DIR).context("creating output directory")?;

            let start = capture
                .started_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let file_stem = format!(
                "profile-{}-{}ms",
                start.as_secs(),
                capture.duration.as_millis()
            );

            let flamegraph_path = PathBuf::from(OUTPUT_DIR).join(format!("{file_stem}.svg"));
            std::fs::write(&flamegraph_path, capture.flamegraph.as_ref())
                .context("writing flamegraph svg")?;

            let profile_path = PathBuf::from(OUTPUT_DIR).join(format!("{file_stem}.pb"));
            std::fs::write(&profile_path, capture.protobuf.as_ref())
                .context("writing raw profile")?;

            Ok(ProfileArtifacts {
                flamegraph_svg: flamegraph_path,
                raw_profile: profile_path,
            })
        })
        .await
        .context("joining profiler export task")?
    }
}
