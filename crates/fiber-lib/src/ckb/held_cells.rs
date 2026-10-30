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
}

#[async_trait::async_trait]
impl<C: CellCollector + Clone> CellCollector for TakingCollector<C> {
    async fn collect_live_cells_async(
        &mut self,
        query: &CellQueryOptions,
        apply_changes: bool,
    ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
        let need = query.min_total_capacity;
        let mut query = query.clone();
        let (mut cells, mut capacity) = (Vec::new(), 0u64);
        for _ in 0..TAKE_ROUNDS {
            let (picked, _) = self
                .inner
                .collect_live_cells_async(&query, apply_changes)
                .await?;
            if picked.is_empty() {
                break;
            }
            // One question per cell, asked together.
            let answers = futures::future::join_all(
                picked.iter().map(|cell| self.pool.spent(&cell.out_point)),
            )
            .await;
            let spent: HashSet<OutPoint> = picked
                .iter()
                .zip(answers)
                .filter(|(_, spent)| *spent)
                .map(|(cell, _)| cell.out_point.clone())
                .collect();
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
                    // Asked again, the collector must not answer with it a second time.
                    if !apply_changes {
                        self.inner.lock_cell(cell.out_point.clone(), u64::MAX)?;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ckb_types::packed::{CellOutput, Script};

    /// The holds are one set for the whole process and every test here changes it, some with
    /// made-up times: one test at a time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
        let _serial = SERIAL.blocking_lock();
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
        let _serial = SERIAL.blocking_lock();
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

    /// A collector over a fixed list of cells, as the indexer is: it answers cells in order until
    /// the query's capacity is met, skipping the locked ones and, after `apply_changes`, the
    /// ones it already gave.
    #[derive(Clone, Default)]
    struct Cells {
        cells: Vec<LiveCell>,
        locked: Vec<OutPoint>,
        used: Vec<OutPoint>,
    }

    impl Cells {
        fn of(cells: &[&OutPoint]) -> Self {
            Self {
                cells: cells
                    .iter()
                    .map(|out_point| live((*out_point).clone()))
                    .collect(),
                ..Default::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl CellCollector for Cells {
        async fn collect_live_cells_async(
            &mut self,
            query: &CellQueryOptions,
            apply_changes: bool,
        ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
            let (mut picked, mut capacity) = (Vec::new(), 0u64);
            for cell in &self.cells {
                if capacity >= query.min_total_capacity {
                    break;
                }
                if self.locked.contains(&cell.out_point) || self.used.contains(&cell.out_point) {
                    continue;
                }
                capacity += Unpack::<u64>::unpack(&cell.output.capacity());
                picked.push(cell.clone());
            }
            if apply_changes {
                self.used
                    .extend(picked.iter().map(|cell| cell.out_point.clone()));
            }
            Ok((picked, capacity))
        }
        fn lock_cell(&mut self, out_point: OutPoint, _: u64) -> Result<(), CellCollectorError> {
            self.locked.push(out_point);
            Ok(())
        }
        fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
            Ok(())
        }
        fn reset(&mut self) {}
    }

    const CELL_CAPACITY: u64 = 100_00000000;

    fn live(out_point: OutPoint) -> LiveCell {
        LiveCell {
            output: CellOutput::new_builder()
                .capacity(ckb_types::core::Capacity::shannons(CELL_CAPACITY).pack())
                .lock(Script::default())
                .build(),
            output_data: Default::default(),
            out_point,
            block_number: 0,
            tx_index: 0,
        }
    }

    fn needing(capacity: u64) -> CellQueryOptions {
        let mut query = CellQueryOptions::new_lock(Script::default());
        query.min_total_capacity = capacity;
        query
    }

    fn out_points(cells: &[LiveCell]) -> Vec<OutPoint> {
        cells.iter().map(|cell| cell.out_point.clone()).collect()
    }

    fn no_pool() -> PoolCheck {
        PoolCheck::Spent(HashSet::new())
    }

    #[cfg(feature = "watchtower")]
    #[tokio::test]
    async fn a_cell_an_opening_picks_is_refused_to_the_watchtower_before_it_is_published() {
        let _serial = SERIAL.lock().await;
        let holder = new_holder();
        let (x, y, z) = (cell(221), cell(222), cell(223));
        let mut funding =
            TakingCollector::with_pool(Cells::of(&[&x, &y]), Taker::Funding(holder), no_pool());
        let (picked, _) = funding
            .collect_live_cells_async(&needing(2 * CELL_CAPACITY), true)
            .await
            .unwrap();
        assert_eq!(out_points(&picked), vec![x.clone(), y.clone()]);

        // The opening has not published its inputs yet (no `hold_funding`): the build is still
        // going on. The watchtower picks from the same live cells now.
        let mut watchtower =
            TakingCollector::with_pool(Cells::of(&[&x, &y, &z]), Taker::Watchtower, no_pool());
        let (picked, capacity) = watchtower
            .collect_live_cells_async(&needing(CELL_CAPACITY), false)
            .await
            .unwrap();
        assert_eq!(
            out_points(&picked),
            vec![z.clone()],
            "x and y are the opening's"
        );
        assert_eq!(capacity, CELL_CAPACITY);

        hold_funding(holder, vec![]);
        release_for_watchtower([z]);
    }

    #[cfg(feature = "watchtower")]
    #[tokio::test]
    async fn a_cell_the_watchtower_picks_is_refused_to_an_opening_until_it_is_released() {
        let _serial = SERIAL.lock().await;
        let (x, y) = (cell(231), cell(232));
        let mut watchtower =
            TakingCollector::with_pool(Cells::of(&[&x]), Taker::Watchtower, no_pool());
        let (picked, _) = watchtower
            .collect_live_cells_async(&needing(CELL_CAPACITY), false)
            .await
            .unwrap();
        assert_eq!(out_points(&picked), vec![x.clone()]);

        let holder = new_holder();
        let mut funding =
            TakingCollector::with_pool(Cells::of(&[&x, &y]), Taker::Funding(holder), no_pool());
        let (picked, _) = funding
            .collect_live_cells_async(&needing(CELL_CAPACITY), true)
            .await
            .unwrap();
        assert_eq!(
            out_points(&picked),
            vec![y.clone()],
            "x is the watchtower's"
        );

        // Its send failed: x is free again.
        release_for_watchtower([x.clone()]);
        let mut funding =
            TakingCollector::with_pool(Cells::of(&[&x]), Taker::Funding(holder), no_pool());
        let (picked, _) = funding
            .collect_live_cells_async(&needing(CELL_CAPACITY), true)
            .await
            .unwrap();
        assert_eq!(out_points(&picked), vec![x]);
        hold_funding(holder, vec![]);
    }

    /// The holds live in memory. After a restart they are gone, but the node's pool still has the
    /// transactions sent before it, and a cell one of them spends is not picked again.
    #[cfg(feature = "watchtower")]
    #[tokio::test]
    async fn a_cell_spent_in_the_pool_is_refused_with_no_hold_here() {
        let _serial = SERIAL.lock().await;
        let (x, y) = (cell(241), cell(242));
        let spent = PoolCheck::Spent(HashSet::from([x.clone()]));

        let holder = new_holder();
        let mut funding =
            TakingCollector::with_pool(Cells::of(&[&x, &y]), Taker::Funding(holder), spent.clone());
        let (picked, _) = funding
            .collect_live_cells_async(&needing(CELL_CAPACITY), true)
            .await
            .unwrap();
        assert_eq!(out_points(&picked), vec![y.clone()]);
        hold_funding(holder, vec![]);

        let mut watchtower =
            TakingCollector::with_pool(Cells::of(&[&x, &y]), Taker::Watchtower, spent);
        let (picked, _) = watchtower
            .collect_live_cells_async(&needing(CELL_CAPACITY), false)
            .await
            .unwrap();
        assert_eq!(out_points(&picked), vec![y.clone()]);
        release_for_watchtower([y]);
    }

    #[tokio::test]
    async fn a_refused_cell_is_replaced_until_the_capacity_is_met() {
        let _serial = SERIAL.lock().await;
        let (x, y, z) = (cell(251), cell(252), cell(253));
        let other = new_holder();
        hold_funding(other, vec![x.clone()]);

        let holder = new_holder();
        let mut funding =
            TakingCollector::with_pool(Cells::of(&[&x, &y, &z]), Taker::Funding(holder), no_pool());
        let (picked, capacity) = funding
            .collect_live_cells_async(&needing(2 * CELL_CAPACITY), true)
            .await
            .unwrap();
        assert_eq!(out_points(&picked), vec![y, z]);
        assert_eq!(capacity, 2 * CELL_CAPACITY);
        hold_funding(holder, vec![]);
        hold_funding(other, vec![]);
    }
}
