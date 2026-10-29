//! The node's own cells that a transaction of its own spends before it is committed, shared by
//! everything here that picks the node's cells. An opening holds the inputs of its funding
//! transaction (the chain actor's `LiveCellsExclusionMap`); the watchtower holds the cells of each
//! revocation and settlement it sends, for `WATCHTOWER_HOLD_MS`. Each one locks all the held cells
//! in its collector. The indexer lists a cell as live until the block that spends it, so without
//! this an opening put into its funding a cell that a settlement in the pool already spent, and
//! the funding was refused (LSP2, 2026-10-28).
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use ckb_sdk::traits::{CellCollector, CellCollectorError};
use ckb_types::packed::OutPoint;

/// How long the watchtower's cells stay held: a transaction of its not committed by then has left
/// the pool, and its cells are free again.
#[cfg(feature = "watchtower")]
pub(crate) const WATCHTOWER_HOLD_MS: u64 = 30 * 60 * 1000;

#[derive(Default)]
struct Held {
    /// The funding inputs of each chain actor, by holder.
    funding: HashMap<u64, Vec<OutPoint>>,
    /// The watchtower's cells, and when each is free again.
    watchtower: HashMap<OutPoint, u64>,
}

static HELD: LazyLock<Mutex<Held>> = LazyLock::new(Default::default);
static NEXT_HOLDER: AtomicU64 = AtomicU64::new(1);

/// A holder for one chain actor's funding inputs.
pub(crate) fn new_holder() -> u64 {
    NEXT_HOLDER.fetch_add(1, Ordering::Relaxed)
}

/// The funding inputs `holder` holds now, in place of the ones it held before.
pub(crate) fn hold_funding(holder: u64, cells: Vec<OutPoint>) {
    // A lock that cannot be taken holds nothing more, as before this module.
    if let Ok(mut held) = HELD.lock() {
        if cells.is_empty() {
            held.funding.remove(&holder);
        } else {
            held.funding.insert(holder, cells);
        }
    }
}

/// The cells of a transaction the watchtower sent, held until `WATCHTOWER_HOLD_MS` after `now_ms`.
#[cfg(feature = "watchtower")]
pub(crate) fn hold_for_watchtower(cells: impl IntoIterator<Item = OutPoint>, now_ms: u64) {
    if let Ok(mut held) = HELD.lock() {
        let until = now_ms.saturating_add(WATCHTOWER_HOLD_MS);
        for cell in cells {
            held.watchtower.insert(cell, until);
        }
    }
}

/// Locks every held cell in `collector`, so that it picks none of them. The watchtower's cells
/// past their hold are let go first.
pub(crate) fn lock_held(
    collector: &mut dyn CellCollector,
    now_ms: u64,
) -> Result<(), CellCollectorError> {
    let cells: Vec<OutPoint> = match HELD.lock() {
        Ok(mut held) => {
            held.watchtower.retain(|_, until| *until > now_ms);
            held.funding
                .values()
                .flatten()
                .chain(held.watchtower.keys())
                .cloned()
                .collect()
        }
        Err(_) => return Ok(()),
    };
    for cell in cells {
        collector.lock_cell(cell, u64::MAX)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ckb_sdk::traits::{CellQueryOptions, LiveCell};
    use ckb_types::{packed::Transaction, prelude::*};

    #[derive(Clone, Default)]
    struct Locks(Vec<OutPoint>);

    #[async_trait::async_trait]
    impl CellCollector for Locks {
        async fn collect_live_cells_async(
            &mut self,
            _query: &CellQueryOptions,
            _apply_changes: bool,
        ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
            Ok((vec![], 0))
        }
        fn lock_cell(&mut self, out_point: OutPoint, _: u64) -> Result<(), CellCollectorError> {
            self.0.push(out_point);
            Ok(())
        }
        fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
            Ok(())
        }
        fn reset(&mut self) {}
    }

    fn cell(n: u8) -> OutPoint {
        OutPoint::new([n; 32].pack(), u32::from(n))
    }

    /// Tests share the holds, so each looks only at its own cells.
    fn locked(of: &[OutPoint], now_ms: u64) -> Vec<OutPoint> {
        let mut locks = Locks::default();
        lock_held(&mut locks, now_ms).unwrap();
        of.iter().filter(|c| locks.0.contains(c)).cloned().collect()
    }

    #[test]
    fn an_openings_inputs_are_locked_until_it_lets_them_go() {
        let holder = new_holder();
        let (a, b) = (cell(201), cell(202));
        hold_funding(holder, vec![a.clone(), b.clone()]);
        assert_eq!(
            locked(&[a.clone(), b.clone()], 0),
            vec![a.clone(), b.clone()]
        );
        // Another opening's holds stay when this one changes.
        let other = new_holder();
        hold_funding(other, vec![cell(203)]);
        hold_funding(holder, vec![b.clone()]);
        assert_eq!(
            locked(&[a.clone(), b.clone(), cell(203)], 0),
            vec![b.clone(), cell(203)]
        );
        hold_funding(holder, vec![]);
        hold_funding(other, vec![]);
        assert!(locked(&[a, b, cell(203)], 0).is_empty());
    }

    #[cfg(feature = "watchtower")]
    #[test]
    fn the_watchtowers_cells_are_locked_for_their_hold() {
        let (c, d) = (cell(211), cell(212));
        hold_for_watchtower([c.clone(), d.clone()], 1_000);
        assert_eq!(
            locked(&[c.clone(), d.clone()], 1_000),
            vec![c.clone(), d.clone()]
        );
        assert_eq!(
            locked(&[c.clone(), d.clone()], 1_000 + WATCHTOWER_HOLD_MS - 1),
            vec![c.clone(), d.clone()]
        );
        assert!(
            locked(&[c, d], 1_000 + WATCHTOWER_HOLD_MS).is_empty(),
            "free again"
        );
    }
}
