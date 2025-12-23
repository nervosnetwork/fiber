use tracing::{debug, warn};

use crate::now_timestamp_as_millis_u64;

pub struct ActorHandleLogGuard {
    actor: &'static str,
    message_name: String,
    start_ms: u64,
    warn_threshold_ms: u64,
    #[cfg(feature = "metrics")]
    metric_name: String,
}

impl ActorHandleLogGuard {
    pub fn new(
        actor: &'static str,
        message_name: String,
        metric_prefix: &str,
        warn_threshold_ms: u64,
    ) -> Self {
        #[cfg(not(feature = "metrics"))]
        let _ = metric_prefix;
        #[cfg(feature = "metrics")]
        let metric_name = format!("{}.{}", metric_prefix, message_name.as_str());

        Self {
            actor,
            message_name,
            start_ms: now_timestamp_as_millis_u64(),
            warn_threshold_ms,
            #[cfg(feature = "metrics")]
            metric_name,
        }
    }
}

impl Drop for ActorHandleLogGuard {
    fn drop(&mut self) {
        let elapsed = now_timestamp_as_millis_u64().saturating_sub(self.start_ms);
        #[cfg(feature = "metrics")]
        {
            let metrics_name = self.metric_name.clone();
            metrics::histogram!(metrics_name).record(elapsed as u32);
        }
        if elapsed > self.warn_threshold_ms {
            warn!(
                actor = self.actor,
                message = %self.message_name,
                elapsed_ms = elapsed,
                "Actor handle took too long"
            );
        } else {
            debug!(
                actor = self.actor,
                message = %self.message_name,
                elapsed_ms = elapsed,
                "Actor handle completed"
            );
        }
    }
}
