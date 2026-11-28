use super::*;

// Cache and subscriber probes used by other modules' tests. They live here
// instead of `tests.rs` because they must exist in every test build: `tests.rs`
// is gated on `watchtower` and `not(wasm32)`, these helpers are not.
impl StoreChangePort {
    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscribers.lock().len()
    }
}

impl Store {
    pub(crate) fn channel_cache_and_record(
        &self,
        id: &Hash256,
    ) -> Option<(ChannelActorState, Option<ChannelActorState>)> {
        self.channel_states().cache_and_record(id)
    }
}

impl Store {
    /// Drop every cached snapshot, as if the store had just been opened (tests only).
    #[cfg(test)]
    pub(crate) fn clear_channel_cache(&self) {
        if let Some(cache) = &self.channel_cache {
            cache.clear();
        }
    }
}

impl Store {
    pub(crate) fn channel_cache_peek(&self, id: &Hash256) -> Option<ChannelActorState> {
        self.channel_states().cache_peek(id)
    }
}

impl Store {
    pub(crate) fn channel_cache_len(&self) -> usize {
        self.channel_states().cache_len()
    }
}
