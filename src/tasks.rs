use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Debug, Clone)]
pub struct TaskTrackerWithCancellation {
    tracker: TaskTracker,
    token: CancellationToken,
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

pub async fn wait_for_tasks_to_finish() {
    TOKIO_TASK_TRACKER_WITH_CANCELLATION.tracker.wait().await;
}
