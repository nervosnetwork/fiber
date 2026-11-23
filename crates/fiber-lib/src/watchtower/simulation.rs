use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use ckb_jsonrpc_types::{
    BlockNumber, CellWithStatus, HeaderView, JsonBytes, OutPoint as JsonOutPoint, OutputsValidator,
    Timestamp, Transaction as JsonTransaction, TransactionWithStatusResponse, Uint32,
};
use ckb_sdk::{
    rpc::ckb_indexer::{
        Cell, CellType, Order, Pagination, ScriptType, SearchKey, SearchMode, Tx, TxWithCell,
        TxWithCells,
    },
    traits::{CellCollector, CellCollectorError, CellQueryOptions, LiveCell},
    RpcError, Since, SinceType,
};
use ckb_types::{
    core::{
        tx_pool::TxStatus, EpochNumberWithFraction, HeaderBuilder, HeaderView as ChainHeader,
        TransactionBuilder, TransactionView,
    },
    packed::{CellOutput, OutPoint, Script, Transaction},
    prelude::*,
    H256,
};
use fiber_types::{Hash256, Privkey};

use crate::ckb::{
    contracts::{get_script_by_contract, Contract},
    signer::LocalSigner,
    tests::test_utils::{CellStatus, MockChainState},
};
use crate::simulation::{ChainEndpoint, SimulatedChainAccess};

use super::chain::WatchtowerChain;

const BLOCK_MS: u64 = 5;
const BLOCKS_PER_EPOCH: u64 = 10;
const FEE_CELL_CAPACITY: u64 = 10_000 * 100_000_000;

#[derive(Clone)]
pub(crate) struct SimulatedWatchtowerChain {
    state: Arc<RwLock<MockChainState>>,
    access: SimulatedChainAccess,
    signer: LocalSigner,
    can_submit: bool,
}

impl SimulatedWatchtowerChain {
    pub(crate) fn new(
        state: Arc<RwLock<MockChainState>>,
        access: SimulatedChainAccess,
        key: Privkey,
        can_submit: bool,
    ) -> anyhow::Result<Self> {
        let signer = LocalSigner::new(key.into());
        if can_submit {
            let lock = get_script_by_contract(Contract::Secp256k1Lock, signer.pubkey_hash());
            let genesis = TransactionBuilder::default()
                .output(
                    CellOutput::new_builder()
                        .capacity(FEE_CELL_CAPACITY)
                        .lock(lock)
                        .build(),
                )
                .output_data(ckb_types::packed::Bytes::default())
                .build();
            let mut chain = state.write().unwrap();
            chain
                .settlement_epoch_ms
                .get_or_insert_with(crate::now_timestamp_as_millis_u64);
            chain.submit_transaction(genesis)?;
        }
        Ok(Self {
            state,
            access,
            signer,
            can_submit,
        })
    }

    pub(crate) fn signer(&self) -> LocalSigner {
        self.signer.clone()
    }

    pub(crate) fn collector(&self) -> SimulatedFeeCollector {
        SimulatedFeeCollector {
            state: self.state.clone(),
            locked: BTreeSet::new(),
        }
    }

    async fn before(&self, request: &'static str) -> Result<(), RpcError> {
        self.access
            .before(ChainEndpoint::Watchtower, request)
            .await
            .map_err(RpcError::Other)
    }
}

impl MockChainState {
    fn simulated_header(&self, number: u64) -> ChainHeader {
        let timestamp =
            self.settlement_epoch_ms.expect("settlement chain clock") + number * BLOCK_MS;
        let epoch = EpochNumberWithFraction::new(
            number / BLOCKS_PER_EPOCH,
            number % BLOCKS_PER_EPOCH,
            BLOCKS_PER_EPOCH,
        );
        HeaderBuilder::default()
            .number(number)
            .timestamp(timestamp)
            .epoch(epoch.full_value())
            .build()
    }

    pub(crate) fn simulated_tip(&self) -> ChainHeader {
        let elapsed = crate::now_timestamp_as_millis_u64()
            .saturating_sub(self.settlement_epoch_ms.expect("settlement chain clock"));
        self.simulated_header(elapsed / BLOCK_MS)
    }

    fn confirmed_transactions(&self) -> impl Iterator<Item = (u64, u32, &TransactionView)> {
        self.txs.values().filter_map(|record| {
            let TxStatus::Committed(block, _, index) = record.tx_status else {
                return None;
            };
            Some((block, index, record.transaction.as_ref()?))
        })
    }

    fn confirmed_cell(&self, outpoint: &OutPoint) -> Option<LiveCell> {
        let record = self.txs.get(&Hash256::from(outpoint.tx_hash()))?;
        let TxStatus::Committed(block_number, _, tx_index) = record.tx_status else {
            return None;
        };
        let index: u32 = outpoint.index().unpack();
        let tx = record.transaction.as_ref()?;
        Some(LiveCell {
            output: tx.outputs().get(index as usize)?,
            output_data: tx.outputs_data().get(index as usize)?.raw_data(),
            out_point: outpoint.clone(),
            block_number,
            tx_index,
        })
    }

    pub(crate) fn live_cells(&self) -> Vec<LiveCell> {
        let mut cells: Vec<_> = self
            .confirmed_transactions()
            .flat_map(|(_, _, tx)| tx.output_pts())
            .filter(|outpoint| !self.cell_status.contains_key(outpoint))
            .filter_map(|outpoint| self.confirmed_cell(&outpoint))
            .collect();
        cells.sort_by_key(|cell| (cell.block_number, cell.tx_index, cell.out_point.clone()));
        cells
    }

    pub(crate) fn validate_simulated_spend(&self, tx: &TransactionView) -> anyhow::Result<()> {
        if tx.inputs().is_empty() {
            // Synthetic funding and fee cells provide the starting value in DST.
            return Ok(());
        }
        let tip = self.simulated_tip();
        let mut total_inputs = 0u128;
        for input in tx.inputs() {
            let cell = self
                .confirmed_cell(&input.previous_output())
                .ok_or_else(|| {
                    anyhow::anyhow!("unknown simulated input {}", input.previous_output())
                })?;
            total_inputs += u128::from(Unpack::<u64>::unpack(&cell.output.capacity()));
            let since = Since::from_raw_value(input.since().unpack());
            anyhow::ensure!(since.flags_is_valid(), "invalid since flags");
            let (metric, value) = since
                .extract_metric()
                .ok_or_else(|| anyhow::anyhow!("invalid since metric"))?;
            let base = self.simulated_header(if since.is_relative() {
                cell.block_number
            } else {
                0
            });
            let mature = match metric {
                SinceType::BlockNumber => tip.number() >= base.number().saturating_add(value),
                SinceType::EpochNumberWithFraction => {
                    let epoch = EpochNumberWithFraction::from_full_value(value).to_rational();
                    tip.epoch().to_rational()
                        >= if since.is_relative() {
                            base.epoch().to_rational() + epoch
                        } else {
                            epoch
                        }
                }
                SinceType::Timestamp => {
                    tip.timestamp()
                        >= value
                            .saturating_mul(1000)
                            .saturating_add(if since.is_relative() {
                                base.timestamp()
                            } else {
                                0
                            })
                }
            };
            anyhow::ensure!(mature, "simulated transaction has an immature input");
        }
        let total_outputs: u128 = tx
            .outputs()
            .into_iter()
            .map(|output| u128::from(Unpack::<u64>::unpack(&output.capacity())))
            .sum();
        anyhow::ensure!(
            total_outputs <= total_inputs,
            "simulated transaction creates capacity"
        );
        Ok(())
    }

    pub(crate) fn simulated_cells(
        &self,
        key: &SearchKey,
        order: Order,
        limit: u32,
        after: Option<JsonBytes>,
    ) -> Pagination<Cell> {
        let cells = self
            .live_cells()
            .into_iter()
            .filter(|cell| {
                matches_block(key, cell.block_number) && matches_script(key, &cell.output)
            })
            .map(|cell| Cell {
                output: cell.output.into(),
                output_data: key
                    .with_data
                    .unwrap_or(true)
                    .then(|| JsonBytes::from_bytes(cell.output_data)),
                out_point: cell.out_point.into(),
                block_number: cell.block_number.into(),
                tx_index: cell.tx_index.into(),
            })
            .collect();
        page(cells, order, limit, after)
    }

    fn simulated_transactions(
        &self,
        key: &SearchKey,
        order: Order,
        limit: u32,
        after: Option<JsonBytes>,
    ) -> Pagination<Tx> {
        let mut transactions: Vec<_> = self.confirmed_transactions().collect();
        transactions.sort_by_key(|(block, index, tx)| (*block, *index, tx.hash()));
        let mut entries = Vec::new();
        for (block, index, tx) in transactions {
            if !matches_block(key, block) {
                continue;
            }
            let mut matches = Vec::new();
            for (i, input) in tx.input_pts_iter().enumerate() {
                if self
                    .confirmed_cell(&input)
                    .is_some_and(|cell| matches_script(key, &cell.output))
                {
                    matches.push((CellType::Input, Uint32::from(i as u32)));
                }
            }
            for (i, output) in tx.outputs().into_iter().enumerate() {
                if matches_script(key, &output) {
                    matches.push((CellType::Output, Uint32::from(i as u32)));
                }
            }
            if matches.is_empty() {
                continue;
            }
            if key.group_by_transaction.unwrap_or_default() {
                entries.push(Tx::Grouped(TxWithCells {
                    tx_hash: tx.hash().into(),
                    block_number: block.into(),
                    tx_index: index.into(),
                    cells: matches,
                }));
            } else {
                entries.extend(matches.into_iter().map(|(io_type, io_index)| {
                    Tx::Ungrouped(TxWithCell {
                        tx_hash: tx.hash().into(),
                        block_number: block.into(),
                        tx_index: index.into(),
                        io_type,
                        io_index,
                    })
                }));
            }
        }
        page(entries, order, limit, after)
    }
}

fn matches_block(key: &SearchKey, block: u64) -> bool {
    key.filter
        .as_ref()
        .and_then(|filter| filter.block_range)
        .is_none_or(|[start, end]| (start.value()..end.value()).contains(&block))
}

fn matches_script(key: &SearchKey, output: &CellOutput) -> bool {
    let script: Script = key.script.clone().into();
    let actual = match key.script_type {
        ScriptType::Lock => output.lock(),
        ScriptType::Type => match output.type_().to_opt() {
            Some(script) => script,
            None => return false,
        },
    };
    if actual.code_hash() != script.code_hash() || actual.hash_type() != script.hash_type() {
        return false;
    }
    let args = script.args().raw_data();
    let actual_args = actual.args().raw_data();
    match key
        .script_search_mode
        .as_ref()
        .unwrap_or(&SearchMode::Prefix)
    {
        SearchMode::Exact => actual_args == args,
        SearchMode::Prefix => actual_args.starts_with(&args),
        SearchMode::Partial => {
            args.is_empty()
                || actual_args
                    .windows(args.len())
                    .any(|slice| slice == args.as_ref())
        }
    }
}

fn page<T>(
    mut entries: Vec<T>,
    order: Order,
    limit: u32,
    after: Option<JsonBytes>,
) -> Pagination<T> {
    if matches!(order, Order::Desc) {
        entries.reverse();
    }
    let start = after
        .as_ref()
        .and_then(|cursor| cursor.as_bytes().try_into().ok())
        .map_or(0, u64::from_le_bytes);
    let objects: Vec<_> = entries
        .into_iter()
        .skip(start as usize)
        .take(limit as usize)
        .collect();
    let next = start + objects.len() as u64;
    Pagination {
        objects,
        last_cursor: JsonBytes::from_vec(next.to_le_bytes().to_vec()),
    }
}

#[async_trait::async_trait]
impl WatchtowerChain for SimulatedWatchtowerChain {
    fn can_submit(&self) -> bool {
        self.can_submit
    }
    async fn get_transaction(
        &self,
        hash: H256,
    ) -> Result<Option<TransactionWithStatusResponse>, RpcError> {
        self.before("get_transaction").await?;
        Ok(self
            .state
            .read()
            .unwrap()
            .txs
            .get(&Hash256::from(hash))
            .and_then(|record| {
                let TxStatus::Committed(block, ref hash, index) = record.tx_status else {
                    return None;
                };
                Some(TransactionWithStatusResponse {
                    transaction: record.transaction.clone().map(|tx| {
                        ckb_jsonrpc_types::ResponseFormat::json(
                            ckb_jsonrpc_types::TransactionView::from(tx),
                        )
                    }),
                    tx_status: ckb_jsonrpc_types::TxStatus::committed(
                        block.into(),
                        hash.clone(),
                        index.into(),
                    ),
                    cycles: None,
                    time_added_to_pool: None,
                    fee: None,
                    min_replace_fee: None,
                })
            }))
    }

    async fn get_tip_header(&self) -> Result<HeaderView, RpcError> {
        self.before("get_tip_header").await?;
        let mut chain = self.state.write().unwrap();
        let tip = chain.simulated_tip();
        chain
            .watchtower_headers
            .insert(tip.hash().into(), tip.timestamp());
        Ok(tip.into())
    }

    async fn get_header_by_number(
        &self,
        number: BlockNumber,
    ) -> Result<Option<HeaderView>, RpcError> {
        self.before("get_header_by_number").await?;
        Ok(Some(
            self.state
                .read()
                .unwrap()
                .simulated_header(number.value())
                .into(),
        ))
    }

    async fn get_block_median_time(&self, hash: H256) -> Result<Option<Timestamp>, RpcError> {
        self.before("get_block_median_time").await?;
        // The tip query and median-time query may be separated by an injected delay.
        Ok(self
            .state
            .read()
            .unwrap()
            .watchtower_headers
            .get(&hash)
            .copied()
            .map(Into::into))
    }

    async fn get_live_cell(
        &self,
        outpoint: JsonOutPoint,
        _: bool,
    ) -> Result<CellWithStatus, RpcError> {
        self.before("get_live_cell").await?;
        let state = self.state.read().unwrap();
        let outpoint: OutPoint = outpoint.into();
        let status = if state.cell_status.get(&outpoint) == Some(&CellStatus::Consumed) {
            "dead"
        } else if state.confirmed_cell(&outpoint).is_some() {
            "live"
        } else {
            "unknown"
        };
        Ok(CellWithStatus {
            cell: None,
            block_hash: None,
            status: status.to_owned(),
        })
    }

    async fn get_cells(
        &self,
        key: SearchKey,
        order: Order,
        limit: Uint32,
        after: Option<JsonBytes>,
    ) -> Result<Pagination<Cell>, RpcError> {
        self.before("get_cells").await?;
        Ok(self
            .state
            .read()
            .unwrap()
            .simulated_cells(&key, order, limit.value(), after))
    }

    async fn get_transactions(
        &self,
        key: SearchKey,
        order: Order,
        limit: Uint32,
        after: Option<JsonBytes>,
    ) -> Result<Pagination<Tx>, RpcError> {
        self.before("get_transactions").await?;
        Ok(self
            .state
            .read()
            .unwrap()
            .simulated_transactions(&key, order, limit.value(), after))
    }

    async fn send_transaction(
        &self,
        tx: JsonTransaction,
        _: Option<OutputsValidator>,
    ) -> Result<H256, RpcError> {
        if !self.can_submit {
            return Err(RpcError::Other(anyhow::anyhow!(
                "read-only watchtower cannot submit transactions"
            )));
        }
        self.before("send_transaction").await?;
        let tx: Transaction = tx.into();
        let tx = tx.into_view();
        let hash = tx.hash().into();
        self.state.write().unwrap().submit_transaction(tx)?;
        Ok(hash)
    }
}

#[derive(Clone)]
pub(crate) struct SimulatedFeeCollector {
    state: Arc<RwLock<MockChainState>>,
    locked: BTreeSet<OutPoint>,
}

#[async_trait::async_trait]
impl CellCollector for SimulatedFeeCollector {
    async fn collect_live_cells_async(
        &mut self,
        query: &CellQueryOptions,
        apply: bool,
    ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
        self.collect_live_cells(query, apply)
    }

    fn collect_live_cells(
        &mut self,
        query: &CellQueryOptions,
        apply: bool,
    ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
        let mut selected = Vec::new();
        let mut capacity = 0u64;
        for cell in self.state.read().unwrap().live_cells() {
            if self.locked.contains(&cell.out_point) || !query.match_cell(&cell, u64::MAX) {
                continue;
            }
            capacity = capacity.saturating_add(cell.output.capacity().unpack());
            selected.push(cell);
            if capacity >= query.min_total_capacity {
                break;
            }
        }
        if apply {
            self.locked
                .extend(selected.iter().map(|cell| cell.out_point.clone()));
        }
        Ok((selected, capacity))
    }

    fn lock_cell(&mut self, out_point: OutPoint, _: u64) -> Result<(), CellCollectorError> {
        self.locked.insert(out_point);
        Ok(())
    }

    fn apply_tx(&mut self, tx: Transaction, _: u64) -> Result<(), CellCollectorError> {
        self.locked.extend(tx.into_view().input_pts_iter());
        Ok(())
    }

    fn reset(&mut self) {
        self.locked.clear();
    }
}
