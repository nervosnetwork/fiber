/// Direction for iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IteratorDirection {
    Forward,
    Reverse,
}

/// A key-value pair returned from iteration.
#[derive(Debug, Clone)]
pub struct KVPair {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// Default batch size for [`PrefixIterator`].
const PREFIX_ITER_BATCH_SIZE: usize = 1000;

/// A lazy, batched iterator over all key-value pairs whose keys share a common
/// prefix.
///
/// Instead of loading every matching entry into memory at once, this iterator
/// fetches [`PREFIX_ITER_BATCH_SIZE`] items at a time via
/// [`StorageBackend::collect_iterator`](crate::backend::StorageBackend::collect_iterator)
/// and yields them one by one. When the current batch is exhausted it
/// automatically fetches the next batch, using the last-seen key (with a `0x00`
/// byte appended) as the start position so that the backend's prefix-seek picks
/// up where we left off.
pub struct PrefixIterator<'a, S: ?Sized> {
    store: &'a S,
    prefix: Vec<u8>,
    /// Current batch of items fetched from the backend.
    buffer: std::vec::IntoIter<KVPair>,
    /// The last key yielded, used to compute the start position for the next
    /// batch fetch.
    last_key: Option<Vec<u8>>,
    /// Whether the previous `collect_iterator` call returned fewer items than
    /// the batch size, which signals that there are no more items to fetch.
    exhausted: bool,
}

impl<'a, S: crate::backend::StorageBackend + ?Sized> PrefixIterator<'a, S> {
    /// Create a new `PrefixIterator` that lazily fetches entries from `store`
    /// whose keys begin with `prefix`.
    pub fn new(store: &'a S, prefix: Vec<u8>) -> Self {
        let mut iter = Self {
            store,
            prefix,
            buffer: Vec::new().into_iter(),
            last_key: None,
            exhausted: false,
        };
        iter.fetch_batch();
        iter
    }

    /// Fetch the next batch of items from the backend.
    fn fetch_batch(&mut self) {
        let start = match self.last_key.take() {
            Some(mut k) => {
                // Start right after the last key we yielded.
                k.push(0x00);
                k
            }
            None => self.prefix.clone(),
        };

        let prefix_clone = self.prefix.clone();
        let batch = self.store.collect_iterator(
            start,
            IteratorDirection::Forward,
            Box::new(move |key: &[u8]| key.starts_with(&prefix_clone)),
            PREFIX_ITER_BATCH_SIZE,
        );

        self.exhausted = batch.len() < PREFIX_ITER_BATCH_SIZE;
        self.buffer = batch.into_iter();
    }
}

impl<S: crate::backend::StorageBackend + ?Sized> Iterator for PrefixIterator<'_, S> {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(kv) = self.buffer.next() {
                self.last_key = Some(kv.key.clone());
                return Some((kv.key, kv.value));
            }

            // Current batch is drained.
            if self.exhausted {
                return None;
            }

            // Fetch the next batch and try again.
            self.fetch_batch();

            // Guard against empty batches to avoid infinite loops.
            if self.exhausted && self.buffer.len() == 0 {
                return None;
            }
        }
    }
}
