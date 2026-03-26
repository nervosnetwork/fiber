use std::future::Future;

use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// Spawn a task on wasm32 using wasm_bindgen_futures
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local as wasm_spawn_local;

#[derive(Debug, Clone)]
pub struct TaskTrackerWithCancellation {
    tracker: TaskTracker,
    token: CancellationToken,
}

impl Default for TaskTrackerWithCancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskTrackerWithCancellation {
    pub fn new() -> Self {
        Self {
            tracker: TaskTracker::new(),
            token: CancellationToken::new(),
        }
    }

    pub async fn close(&self) {
        self.token.cancel();
        self.tracker.close();
        self.tracker.wait().await;
    }
}

static TOKIO_TASK_TRACKER_WITH_CANCELLATION: once_cell::sync::Lazy<TaskTrackerWithCancellation> =
    once_cell::sync::Lazy::new(TaskTrackerWithCancellation::new);

/// Create a new CancellationToken for exit signal
pub fn new_tokio_cancellation_token() -> CancellationToken {
    TOKIO_TASK_TRACKER_WITH_CANCELLATION.token.clone()
}

/// Create a new TaskTracker to track task progress
pub fn new_tokio_task_tracker() -> TaskTracker {
    TOKIO_TASK_TRACKER_WITH_CANCELLATION.tracker.clone()
}

/// Shutdown all tasks, and wait for their completion.
pub async fn cancel_tasks_and_wait_for_completion() {
    TOKIO_TASK_TRACKER_WITH_CANCELLATION.close().await;
}

/// Spawn a task on non-wasm32 platform
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn<F>(fut: F)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    TOKIO_TASK_TRACKER_WITH_CANCELLATION.tracker.spawn(fut);
}

/// Spawn a task on wasm32
/// Note: In WASM environment, we use wasm_bindgen_futures::spawn_local instead of
/// tokio_util's spawn_local because the latter requires a LocalSet context which
/// is not available in the browser's event loop.
#[cfg(target_arch = "wasm32")]
pub fn spawn<F>(fut: F)
where
    F: Future<Output = ()> + 'static,
{
    wasm_spawn_local(fut);
}
