use super::*;

impl ChannelUpdateLimiter {
    pub(crate) fn tracked_keys(&self) -> usize {
        self.buckets.len()
    }
}

impl ChannelUpdateRateLimitConfig {
    pub(crate) fn build_limiter_with_max_entries(
        &self,
        max_entries: usize,
    ) -> ChannelUpdateLimiter {
        ChannelUpdateLimiter::with_max_entries(self.clone(), max_entries)
    }
}
