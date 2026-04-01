use fiber_store::backend::StorageBackend;
use fiber_store::iterator::{IteratorDirection, KVPair};

/// Builder for prefix iteration options, modelled after `std::fs::OpenOptions`.
///
/// # Examples
///
/// ```ignore
/// // Limit to 1 result:
/// self.collect_by_prefix_with(&prefix, PrefixIterOptions::new().limit(1))
///
/// // Reverse scan with limit:
/// self.collect_by_prefix_with(&prefix, PrefixIterOptions::new().reverse().limit(1))
///
/// // Paginated forward scan with exclusive cursor:
/// self.collect_by_prefix_with(&prefix, PrefixIterOptions::new()
///     .start_key(&cursor_key)
///     .start_key_exclusive())
/// ```
pub struct PrefixIterOptions<'a> {
    pub(crate) direction: IteratorDirection,
    pub(crate) start_key: Option<&'a [u8]>,
    pub(crate) start_key_exclusive: bool,
    pub(crate) limit: usize,
}

impl<'a> PrefixIterOptions<'a> {
    /// Create options with defaults: forward direction, no start key, no limit.
    pub fn new() -> Self {
        Self {
            direction: IteratorDirection::Forward,
            start_key: None,
            start_key_exclusive: false,
            limit: 0,
        }
    }

    /// Set the iteration direction to reverse.
    pub fn reverse(mut self) -> Self {
        self.direction = IteratorDirection::Reverse;
        self
    }

    /// Start iteration from the given key instead of the prefix boundary.
    pub fn start_key(mut self, key: &'a [u8]) -> Self {
        self.start_key = Some(key);
        self
    }

    /// Make the start key exclusive — the entry matching start_key itself
    /// will be skipped. Only meaningful when `start_key` is set.
    pub fn start_key_exclusive(mut self) -> Self {
        self.start_key_exclusive = true;
        self
    }

    /// Cap the number of entries returned. 0 means no limit (the default).
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = n;
        self
    }
}

impl Default for PrefixIterOptions<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait that extends `StorageBackend` with convenience iterator methods
/// used by the domain store trait implementations.
///
/// The `fiber_store::Store` type (which cfg-selects between RocksDB, IndexedDB,
/// BTreeMap, or SQLite backends) implements this trait, and the domain trait
/// implementations (`ChannelActorStateStore`, `InvoiceStore`, etc.) are written
/// in terms of these convenience methods.
pub trait FiberStore: StorageBackend + Clone + std::fmt::Debug {
    /// Collect all entries whose key starts with `prefix`, in forward order.
    fn collect_by_prefix(&self, prefix: &[u8]) -> Vec<KVPair> {
        let prefix_owned = prefix.to_vec();
        self.collect_iterator(
            prefix.to_vec(),
            IteratorDirection::Forward,
            Box::new(move |key| key.starts_with(&prefix_owned)),
            0,
        )
    }

    /// Collect entries whose key starts with `prefix`, governed by `options`.
    ///
    /// `start_key` and `direction` semantics:
    /// - `start_key = None, Forward` → iterate from prefix start
    /// - `start_key = None, Reverse` → iterate from prefix end (last entry first)
    /// - `start_key = Some(key), Forward` → iterate forward from key
    /// - `start_key = Some(key), Reverse` → iterate backward from key
    fn collect_by_prefix_with(&self, prefix: &[u8], options: PrefixIterOptions<'_>) -> Vec<KVPair> {
        let PrefixIterOptions {
            direction,
            start_key,
            start_key_exclusive,
            limit,
        } = options;

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

        // When start_key_exclusive is set, fetch one extra entry so the cursor
        // entry doesn't consume a slot that should belong to real results.
        let fetch_limit = if start_key_exclusive && start_key.is_some() && limit > 0 {
            limit + 1
        } else {
            limit
        };

        let start_key_owned = if start_key_exclusive {
            start_key.map(|k| k.to_vec())
        } else {
            None
        };

        let mut results = self.collect_iterator(
            start,
            direction,
            Box::new(move |key: &[u8]| key.starts_with(&prefix_owned)),
            fetch_limit,
        );

        // Strip the cursor entry itself when exclusive.
        // The cursor key is always the first element returned by collect_iterator
        // because iteration starts from start_key in both forward and reverse.
        if let Some(ref cursor_key) = start_key_owned {
            if results.first().is_some_and(|kv| kv.key == *cursor_key) {
                results.remove(0);
            }
            // Truncate back to the original limit.
            if limit > 0 && results.len() > limit {
                results.truncate(limit);
            }
        }

        results
    }
}

// The wrapper Store in store_impl implements StorageBackend by delegating to
// fiber_store::Store. This impl gives it all the FiberStore convenience methods.
impl FiberStore for crate::store::store_impl::Store {}
