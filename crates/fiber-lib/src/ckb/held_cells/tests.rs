use super::TEST_SERIAL as SERIAL;
use super::*;
use ckb_types::packed::{CellOutput, Script};

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
        .collect_live_cells_async(&needing(CELL_CAPACITY), true)
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
    let mut watchtower = TakingCollector::with_pool(Cells::of(&[&x]), Taker::Watchtower, no_pool());
    let (picked, _) = watchtower
        .collect_live_cells_async(&needing(CELL_CAPACITY), true)
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

    let mut watchtower = TakingCollector::with_pool(Cells::of(&[&x, &y]), Taker::Watchtower, spent);
    let (picked, _) = watchtower
        .collect_live_cells_async(&needing(CELL_CAPACITY), true)
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

/// The SDK's capacity balancer peeks (`apply_changes` false) for one more cell when the change
/// is too small, then asks again with `apply_changes` to consume it. The peek takes nothing,
/// so the cell it saw is still there to consume.
#[tokio::test]
async fn a_peeked_cell_can_then_be_consumed() {
    let _serial = SERIAL.lock().await;
    let x = cell(181);
    let holder = new_holder();
    let mut funding =
        TakingCollector::with_pool(Cells::of(&[&x]), Taker::Funding(holder), no_pool());
    let (peeked, _) = funding
        .collect_live_cells_async(&needing(CELL_CAPACITY), false)
        .await
        .unwrap();
    assert_eq!(out_points(&peeked), vec![x.clone()]);

    let (taken, capacity) = funding
        .collect_live_cells_async(&needing(CELL_CAPACITY), true)
        .await
        .unwrap();
    assert_eq!(
        out_points(&taken),
        vec![x.clone()],
        "the peeked cell is consumed"
    );
    assert_eq!(capacity, CELL_CAPACITY);
    assert_eq!(
        locked(std::slice::from_ref(&x), 0),
        vec![x],
        "consumed, it is held"
    );
    hold_funding(holder, vec![]);
}

#[tokio::test]
async fn a_peek_still_skips_a_cell_another_opening_holds() {
    let _serial = SERIAL.lock().await;
    let (x, y) = (cell(191), cell(192));
    let other = new_holder();
    hold_funding(other, vec![x.clone()]);

    let holder = new_holder();
    let mut funding =
        TakingCollector::with_pool(Cells::of(&[&x, &y]), Taker::Funding(holder), no_pool());
    let (peeked, capacity) = funding
        .collect_live_cells_async(&needing(CELL_CAPACITY), false)
        .await
        .unwrap();
    assert_eq!(
        out_points(&peeked),
        vec![y.clone()],
        "x is the other opening's"
    );
    assert_eq!(capacity, CELL_CAPACITY);
    assert!(locked(&[y], 0).is_empty(), "the peek holds nothing");
    hold_funding(other, vec![]);
}
