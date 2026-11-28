//! The node's own cells that a transaction of its own spends before it is committed, shared by
//! everything here that picks the node's cells. An opening holds the inputs of its funding
//! transaction (the chain actor's `LiveCellsExclusionMap`); the watchtower holds the cells of each
//! revocation and settlement it sends, for `WATCHTOWER_HOLD_MS`. The indexer lists a cell as live
//! until the block that spends it, so without this an opening put into its funding a cell that a
//! settlement in the pool already spent, and the funding was refused (LSP2, 2026-10-28).
//!
//! Both pick through `TakingCollector`, which holds a cell in the same step that checks it is free
//! (`take`), so the two cannot pick the same cell between a pick and its hold. It also asks the
//! node whether a transaction in its pool already spends a cell: the holds live in memory, and
//! after a restart the pool still knows what our earlier transactions spend.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use ckb_sdk::traits::{CellCollector, CellCollectorError, CellQueryOptions, LiveCell};
use ckb_types::packed::{OutPoint, Transaction};
use ckb_types::prelude::*;

/// How long the watchtower's cells stay held: a transaction of its not committed by then has left
/// the pool, and its cells are free again.
#[cfg(feature = "watchtower")]
pub(crate) const WATCHTOWER_HOLD_MS: u64 = 30 * 60 * 1000;

/// How long a cell the watchtower took stays held while its transaction is built and sent. One
/// that is never sent gives its cells back by then; one that is sent holds them for
/// `WATCHTOWER_HOLD_MS` (`hold_for_watchtower`).
#[cfg(feature = "watchtower")]
pub(crate) const WATCHTOWER_PICK_HOLD_MS: u64 = 60 * 1000;

/// How many times a collector is asked again for cells after some were refused.
const TAKE_ROUNDS: usize = 8;

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

/// The cells of a transaction the watchtower could not send, free again.
#[cfg(feature = "watchtower")]
pub(crate) fn release_for_watchtower(cells: impl IntoIterator<Item = OutPoint>) {
    if let Ok(mut held) = HELD.lock() {
        for cell in cells {
            held.watchtower.remove(&cell);
        }
    }
}

/// Who takes cells: the chain actor that builds an opening's funding, by its holder, or the
/// watchtower.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Taker {
    Funding(u64),
    #[cfg(feature = "watchtower")]
    Watchtower,
}

/// Takes the cells `picked` for `taker`, in one step under the lock every taker goes through: a
/// cell held here already, or one in `spent` (spent by a transaction in the node's pool), is
/// refused, and every other one is held for `taker` before this returns. Two takers can then never
/// both get a cell. Returns the taken cells and the refused ones.
pub(crate) fn take(
    picked: Vec<OutPoint>,
    spent: &HashSet<OutPoint>,
    taker: Taker,
    now_ms: u64,
) -> (Vec<OutPoint>, Vec<OutPoint>) {
    // A lock that cannot be taken refuses nothing and holds nothing, as before this module.
    let Ok(mut held) = HELD.lock() else {
        return (picked, vec![]);
    };
    held.watchtower.retain(|_, until| *until > now_ms);
    let busy: HashSet<OutPoint> = held
        .funding
        .values()
        .flatten()
        .chain(held.watchtower.keys())
        .cloned()
        .collect();
    let (mut taken, mut refused) = (Vec::new(), Vec::new());
    for cell in picked {
        if busy.contains(&cell) || spent.contains(&cell) || taken.contains(&cell) {
            refused.push(cell);
            continue;
        }
        match taker {
            Taker::Funding(holder) => held.funding.entry(holder).or_default().push(cell.clone()),
            #[cfg(feature = "watchtower")]
            Taker::Watchtower => {
                held.watchtower
                    .insert(cell.clone(), now_ms.saturating_add(WATCHTOWER_PICK_HOLD_MS));
            }
        }
        taken.push(cell);
    }
    (taken, refused)
}

/// The cells of `picked` that `take` would refuse now (held here, or in `spent`), without holding
/// any. A lock that cannot be taken refuses nothing, as in `take`.
fn unavailable<'a>(
    picked: impl Iterator<Item = &'a OutPoint>,
    spent: &HashSet<OutPoint>,
    now_ms: u64,
) -> HashSet<OutPoint> {
    let Ok(held) = HELD.lock() else {
        return HashSet::new();
    };
    picked
        .filter(|cell| {
            spent.contains(*cell)
                || held.funding.values().flatten().any(|held| held == *cell)
                || held
                    .watchtower
                    .get(*cell)
                    .is_some_and(|until| *until > now_ms)
        })
        .cloned()
        .collect()
}

/// Where `TakingCollector` learns whether a transaction in the node's pool already spends a cell.
#[derive(Clone)]
pub(crate) enum PoolCheck {
    /// The node at this RPC address.
    Node(String),
    /// These cells, and no others.
    #[cfg(test)]
    Spent(HashSet<OutPoint>),
}

impl PoolCheck {
    /// Whether a transaction in the pool spends `cell`, or the cell is gone. False when the node
    /// cannot be asked, so a node that cannot answer picks cells as before.
    async fn spent(&self, cell: &OutPoint) -> bool {
        match self {
            PoolCheck::Node(rpc_url) => {
                match crate::ckb::config::new_ckb_rpc_async_client(rpc_url)
                    .get_live_cell_with_include_tx_pool(cell.clone().into(), false, true)
                    .await
                {
                    Ok(cell) => cell.status != "live",
                    Err(_) => false,
                }
            }
            #[cfg(test)]
            PoolCheck::Spent(cells) => cells.contains(cell),
        }
    }
}

/// A cell collector whose every pick is taken for one taker (`take`): a cell held here or spent in
/// the pool is locked in the inner collector and skipped, and the inner collector is asked again
/// until the query's capacity is met or it has no more cells. A taken cell is held from the moment
/// it is picked, so the other taker no longer sees it.
///
/// Cells are taken only when `apply_changes` is set. Without it the collector previews: it answers
/// the cells it would give, less the held and spent ones, and takes none, so a caller can peek and
/// then consume the same cell (the SDK's capacity balancer does this when the change is too small).
#[derive(Clone)]
pub(crate) struct TakingCollector<C> {
    inner: C,
    taker: Taker,
    pool: PoolCheck,
}

impl<C> TakingCollector<C> {
    /// Takes for `taker`, asking the node at `rpc_url` about its pool.
    pub(crate) fn new(inner: C, taker: Taker, rpc_url: String) -> Self {
        Self::with_pool(inner, taker, PoolCheck::Node(rpc_url))
    }

    pub(crate) fn with_pool(inner: C, taker: Taker, pool: PoolCheck) -> Self {
        Self { inner, taker, pool }
    }

    /// The cells of `picked` that a transaction in the node's pool spends, asked together.
    async fn spent_in_pool(&self, picked: &[LiveCell]) -> HashSet<OutPoint> {
        let answers =
            futures::future::join_all(picked.iter().map(|cell| self.pool.spent(&cell.out_point)))
                .await;
        picked
            .iter()
            .zip(answers)
            .filter(|(_, spent)| *spent)
            .map(|(cell, _)| cell.out_point.clone())
            .collect()
    }
}

impl<C: CellCollector + Clone> TakingCollector<C> {
    /// A preview: the cells the inner collector would give for `query`, less the held and spent
    /// ones, with nothing taken. A refused cell is locked in the inner collector so the next ask
    /// offers another; it could not be taken anyway.
    async fn peek(
        &mut self,
        query: &CellQueryOptions,
    ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
        let mut answer = (Vec::new(), 0u64);
        for _ in 0..TAKE_ROUNDS {
            let (picked, _) = self.inner.collect_live_cells_async(query, false).await?;
            let spent = self.spent_in_pool(&picked).await;
            let refused = unavailable(
                picked.iter().map(|cell| &cell.out_point),
                &spent,
                crate::now_timestamp_as_millis_u64(),
            );
            let cells: Vec<LiveCell> = picked
                .into_iter()
                .filter(|cell| !refused.contains(&cell.out_point))
                .collect();
            let capacity = cells.iter().fold(0u64, |sum, cell| {
                sum.saturating_add(Unpack::<u64>::unpack(&cell.output.capacity()))
            });
            answer = (cells, capacity);
            if refused.is_empty() {
                break;
            }
            for cell in refused {
                self.inner.lock_cell(cell, u64::MAX)?;
            }
        }
        Ok(answer)
    }
}

#[async_trait::async_trait]
impl<C: CellCollector + Clone> CellCollector for TakingCollector<C> {
    async fn collect_live_cells_async(
        &mut self,
        query: &CellQueryOptions,
        apply_changes: bool,
    ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
        if !apply_changes {
            return self.peek(query).await;
        }
        let need = query.min_total_capacity;
        let mut query = query.clone();
        let (mut cells, mut capacity) = (Vec::new(), 0u64);
        for _ in 0..TAKE_ROUNDS {
            let (picked, _) = self.inner.collect_live_cells_async(&query, true).await?;
            if picked.is_empty() {
                break;
            }
            let spent = self.spent_in_pool(&picked).await;
            let (taken, refused) = take(
                picked.iter().map(|cell| cell.out_point.clone()).collect(),
                &spent,
                self.taker,
                crate::now_timestamp_as_millis_u64(),
            );
            for cell in &refused {
                self.inner.lock_cell(cell.clone(), u64::MAX)?;
            }
            for cell in picked {
                if taken.contains(&cell.out_point) {
                    let cell_capacity: u64 = cell.output.capacity().unpack();
                    capacity = capacity.saturating_add(cell_capacity);
                    cells.push(cell);
                }
            }
            if refused.is_empty() || capacity >= need {
                break;
            }
            query.min_total_capacity = need - capacity;
        }
        Ok((cells, capacity))
    }

    fn lock_cell(
        &mut self,
        out_point: OutPoint,
        tip_block_number: u64,
    ) -> Result<(), CellCollectorError> {
        self.inner.lock_cell(out_point, tip_block_number)
    }

    fn apply_tx(
        &mut self,
        tx: Transaction,
        tip_block_number: u64,
    ) -> Result<(), CellCollectorError> {
        self.inner.apply_tx(tx, tip_block_number)
    }

    fn reset(&mut self) {
        self.inner.reset()
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

/// The holds are one set for the whole process and tests that inspect or mutate it with
/// synthetic or real timestamps must serialize on this mutex.
#[cfg(test)]
pub(crate) static TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests;
