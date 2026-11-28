use super::*;

impl<'a> ChannelStates<'a> {
    /// The cached snapshot only, without falling back to the record (tests only).
    #[cfg(test)]
    pub(crate) fn cache_peek(&self, id: &Hash256) -> Option<ChannelActorState> {
        let key = self.cache_key(id);
        self.cache
            .and_then(|cache| cache.peek(&key))
            .map(|hit| (*hit).clone())
    }
}

impl<'a> ChannelStates<'a> {
    /// The cached snapshot together with the stored record, both read while the slot's read lock
    /// is held, so a concurrent write cannot change the record between the two reads (tests only).
    #[cfg(test)]
    pub(crate) fn cache_and_record(
        &self,
        id: &Hash256,
    ) -> Option<(ChannelActorState, Option<ChannelActorState>)> {
        let cache = self.cache?;
        let key = self.cache_key(id);
        let entry = cache.entries.get(&key)?;
        let slot = entry.try_read()?;
        let cached = (**slot.as_ref()?).clone();
        let record = self.load(id);
        Some((cached, record))
    }
}

impl<'a> ChannelStates<'a> {
    pub(crate) fn cache_len(&self) -> usize {
        self.cache.map_or(0, |cache| {
            let ns = self.store.namespace();
            cache
                .entries
                .iter()
                .filter(|entry| entry.key().0.as_ref() == ns)
                .count()
        })
    }
}
