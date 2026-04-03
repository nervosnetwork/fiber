use crate::iterator::{IteratorDirection, KVPair, PrefixIterator};

/// A function that determines whether to keep taking items during iteration.
/// Returns `true` to continue taking, `false` to stop.
pub type TakeWhileFn = Box<dyn Fn(&[u8]) -> bool + Send + 'static>;

/// A batch of write operations to be applied atomically.
pub trait BatchWriter {
    /// Queue a put operation in the batch.
    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, key: K, value: V);

    /// Queue a delete operation in the batch.
    fn delete<K: AsRef<[u8]>>(&mut self, key: K);

    /// Atomically commit all queued operations.
    fn commit(self);
}

/// Unified key-value storage backend trait.
///
/// All methods panic on I/O errors.
pub trait StorageBackend: Send + Sync {
    /// The batch type for this backend.
    type Batch: BatchWriter;

    /// Get a value by key.
    fn get<K: AsRef<[u8]>>(&self, key: K) -> Option<Vec<u8>>;

    /// Put a key-value pair.
    fn put<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V);

    /// Delete a key.
    fn delete<K: AsRef<[u8]>>(&self, key: K);

    /// Create a new batch writer.
    fn batch(&self) -> Self::Batch;

    /// Collect items from an iterator over the key-value store.
    ///
    /// - `start`: The key to start iterating from (inclusive).
    /// - `direction`: Forward or reverse iteration.
    /// - `take_while_fn`: Called with each key; iteration stops when this returns `false`.
    /// - `limit`: Maximum number of items to return (0 = no limit).
    fn collect_iterator(
        &self,
        start: Vec<u8>,
        direction: IteratorDirection,
        take_while_fn: TakeWhileFn,
        limit: usize,
    ) -> Vec<KVPair>;

    /// Return a lazy iterator over all key-value pairs whose keys start with
    /// `prefix`.
    ///
    /// This is primarily used by the migration tool (`fnn-migrate`) and
    /// `check_validate` to scan store prefixes without loading every entry into
    /// memory at once. Normal business logic should use higher-level query
    /// methods instead.
    ///
    /// The default implementation batches calls to [`Self::collect_iterator`]
    /// so that only a bounded number of entries are held in memory at any time.
    fn prefix_iterator(&self, prefix: &[u8]) -> PrefixIterator<'_, Self> {
        PrefixIterator::new(self, prefix.to_vec())
    }
}
