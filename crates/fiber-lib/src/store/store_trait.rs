use fiber_store::backend::StorageBackend;
use fiber_store::iterator::{IteratorDirection, KVPair};

/// Type alias for the `skip_while` predicate used by `prefix_iterator_with_skip_while_and_start`.
pub type SkipWhileFn = Box<dyn Fn(&[u8]) -> bool + Send + 'static>;

/// Trait that extends `StorageBackend` with convenience iterator methods
/// used by the domain store trait implementations.
///
/// The `fiber_store::Store` type (which cfg-selects between RocksDB, IndexedDB,
/// BTreeMap, or SQLite backends) implements this trait, and the domain trait
/// implementations (`ChannelActorStateStore`, `InvoiceStore`, etc.) are written
/// in terms of these convenience methods.
pub trait FiberStore: StorageBackend + Clone + std::fmt::Debug {
    /// Iterate all entries whose key starts with `prefix`, in forward order.
    ///
    /// Returns an owned iterator that does not borrow any input parameters.
    fn prefix_iterator(&self, prefix: &[u8]) -> std::vec::IntoIter<KVPair> {
        self.prefix_iterator_with_limit(prefix, 0)
    }

    /// Like `prefix_iterator`, but caps the number of entries returned.
    ///
    /// `limit` = 0 means no limit.
    fn prefix_iterator_with_limit(
        &self,
        prefix: &[u8],
        limit: usize,
    ) -> std::vec::IntoIter<KVPair> {
        let prefix_owned = prefix.to_vec();
        self.collect_iterator(
            prefix.to_vec(),
            IteratorDirection::Forward,
            Box::new(move |key| key.starts_with(&prefix_owned)),
            limit,
        )
        .into_iter()
    }

    /// Iterate entries with `prefix`, starting from `start_key` in the given `direction`,
    /// skipping entries while `skip_while` returns true.
    ///
    /// `start_key` and `direction` semantics:
    /// - `start_key = None, direction = Forward` → iterate from prefix start
    /// - `start_key = None, direction = Reverse` → iterate from prefix end (last entry first)
    /// - `start_key = Some(key), direction = Forward` → iterate forward from key
    /// - `start_key = Some(key), direction = Reverse` → iterate backward from key
    ///
    /// Returns an owned iterator that does not borrow any input parameters.
    fn prefix_iterator_with_skip_while_and_start(
        &self,
        prefix: &[u8],
        start_key: Option<&[u8]>,
        direction: IteratorDirection,
        skip_while: SkipWhileFn,
    ) -> std::vec::IntoIter<KVPair> {
        self.prefix_iterator_with_skip_while_and_start_and_limit(
            prefix, start_key, direction, skip_while, 0,
        )
    }

    /// Like `prefix_iterator_with_skip_while_and_start`, but caps the number
    /// of entries collected from the backend *before* `skip_while` filtering.
    ///
    /// `limit` = 0 means no limit. When the caller knows both a `skip_while`
    /// predicate and a result-count cap, pass the sum of the two as the limit
    /// for an upper bound, or 0 to be safe.
    fn prefix_iterator_with_skip_while_and_start_and_limit(
        &self,
        prefix: &[u8],
        start_key: Option<&[u8]>,
        direction: IteratorDirection,
        skip_while: SkipWhileFn,
        limit: usize,
    ) -> std::vec::IntoIter<KVPair> {
        let prefix_owned = prefix.to_vec();

        let start = match (start_key, direction) {
            (Some(key), _) => key.to_vec(),
            (None, IteratorDirection::Forward) => prefix.to_vec(),
            (None, IteratorDirection::Reverse) => {
                // For reverse iteration, start past the prefix range.
                // Compute prefix + 1 (big-endian increment with carry).
                let mut end_key = prefix.to_vec();
                let mut carried = true;
                for byte in end_key.iter_mut().rev() {
                    if *byte < 0xFF {
                        *byte += 1;
                        carried = false;
                        break;
                    } else {
                        *byte = 0x00;
                    }
                }
                if carried {
                    // Prefix was all 0xFF — use a key that sorts after
                    // every possible extension of the prefix.
                    end_key = vec![0xFF; prefix.len() + 1];
                }
                end_key
            }
        };

        let results = self.collect_iterator(
            start,
            direction,
            Box::new(move |key: &[u8]| key.starts_with(&prefix_owned)),
            limit,
        );

        // Apply skip_while post-collection.
        let mut skipping = true;
        results
            .into_iter()
            .filter(move |kv| {
                if skipping {
                    if skip_while(&kv.key) {
                        return false;
                    }
                    skipping = false;
                }
                true
            })
            .collect::<Vec<_>>()
            .into_iter()
    }
}

// The wrapper Store in store_impl implements StorageBackend by delegating to
// fiber_store::Store. This impl gives it all the FiberStore convenience methods.
impl FiberStore for crate::store::store_impl::Store {}
