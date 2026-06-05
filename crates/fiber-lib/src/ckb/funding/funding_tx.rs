use super::super::FundingError;
use super::limits::validate_peer_funding_tx_complexity;
use crate::ckb::{
    config::{new_ckb_rpc_async_client, new_default_cell_collector},
    contracts::get_udt_cell_deps,
};

use anyhow::anyhow;
use ckb_sdk::{
    constants::SIGHASH_TYPE_HASH,
    rpc::ckb_indexer::SearchMode,
    traits::{
        CellCollector, CellDepResolver, CellQueryOptions, DefaultCellDepResolver,
        DefaultHeaderDepResolver, DefaultTransactionDependencyProvider, HeaderDepResolver,
        SecpCkbRawKeySigner, TransactionDependencyProvider, ValueRangeOption,
    },
    tx_builder::{unlock_tx_async, CapacityBalancer, TxBuilder, TxBuilderError},
    unlock::{ScriptUnlocker, SecpSighashUnlocker},
    ScriptId,
};
use ckb_types::{
    core::{BlockView, Capacity, TransactionView},
    packed::{self, Bytes, CellInput, CellOutput, Script, Transaction},
    prelude::*,
};
use fiber_types::serde_utils::EntityHex;
use molecule::{
    bytes::{BufMut as _, BytesMut},
    prelude::*,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::{HashMap, HashSet};
use tracing::{debug, trace};

// Number of blocks to keep the committed funding tx in the exclusion map.
// It is the same with the value used in the CKB SDK.
const KEEP_BLOCK_PERIOD: u64 = 13;

const INSUFFICIENT_UDT_CELLS_MSG: &str =
    "can not find enough UDT owner cells for funding transaction";

pub(crate) fn map_tx_builder_error(e: TxBuilderError) -> FundingError {
    if e.to_string().contains(INSUFFICIENT_UDT_CELLS_MSG) {
        FundingError::InsufficientCells(e.to_string())
    } else {
        FundingError::from(e)
    }
}

// SecpSighash uses a 65-byte recoverable signature in `WitnessArgs.lock`:
// 64-byte compact signature + 1-byte recovery id. The full serialized witness
// is larger because it also includes Molecule table/bytes headers.
pub(crate) const SECP_SIGHASH_PLACEHOLDER_SIGNATURE_BYTES: usize = 65;

pub(crate) fn secp_sighash_placeholder_witness() -> packed::WitnessArgs {
    packed::WitnessArgs::new_builder()
        .lock(
            Some(molecule::bytes::Bytes::from(vec![
                0u8;
                SECP_SIGHASH_PLACEHOLDER_SIGNATURE_BYTES
            ]))
            .pack(),
        )
        .build()
}

pub(crate) fn is_secp_sighash_placeholder_witness(witness: &[u8]) -> bool {
    witness == secp_sighash_placeholder_witness().as_slice()
}

/// Funding transaction wrapper.
///
/// It includes extra fields to verify the transaction.
#[derive(Clone, Debug, Default)]
pub struct FundingTx {
    tx: Option<TransactionView>,
}

#[derive(Debug, Clone)]
pub(crate) struct LiveCellsExclusion {
    pub(crate) input_out_points: Vec<packed::OutPoint>,
    committed_block_number: Option<u64>,
}

#[derive(Debug, Default, Clone)]
pub struct LiveCellsExclusionMap {
    pub(crate) map: HashMap<packed::Byte32, LiveCellsExclusion>,
}

impl LiveCellsExclusionMap {
    pub fn new() -> Self {
        Self {
            map: HashMap::default(),
        }
    }

    /// Remove the committed funding txs that has been committed KEEP_BLOCK_PERIOD blocks ago.
    pub fn truncate(&mut self, tip_block_number: u64) {
        self.map.retain(|_, exclusion| {
            if let Some(committed_block_number) = exclusion.committed_block_number {
                tip_block_number
                    .checked_sub(committed_block_number)
                    .is_none_or(|age| age < KEEP_BLOCK_PERIOD)
            } else {
                true
            }
        });
    }

    pub fn add_transaction_view(&mut self, tx: &TransactionView) {
        let tx_hash = tx.hash();
        let exclusion = LiveCellsExclusion {
            input_out_points: tx.input_pts_iter().collect(),
            committed_block_number: None,
        };
        self.map.insert(tx_hash, exclusion);
    }

    pub fn add_funding_tx(&mut self, tx: &FundingTx) {
        if let Some(tx) = tx.tx.as_ref() {
            self.add_transaction_view(tx);
        }
    }

    pub fn commit(&mut self, tx_hash: &packed::Byte32, block_number: u64) {
        if let Some(exclusion) = self.map.get_mut(tx_hash) {
            exclusion.committed_block_number = Some(block_number);
        }
    }

    pub fn remove(&mut self, tx_hash: &packed::Byte32) {
        self.map.remove(tx_hash);
    }

    /// Move the exclusion entry for a funding tx that a peer replaced via `TxUpdate`.
    ///
    /// When the local node builds a funding tx, its inputs are reserved under that local tx
    /// hash. A peer can answer with a verified `TxUpdate` that keeps those inputs but produces a
    /// different tx hash, after which the channel persists only the new hash. Without moving the
    /// entry here, the old hash lingers with `committed_block_number == None`, so `truncate`
    /// keeps it forever and abort/timeout cleanup (which only removes the latest persisted hash)
    /// never releases the reserved cells.
    ///
    /// The entry is only moved when one exists for `old_tx_hash`, preserving the previous
    /// behavior for sides that never reserved local cells under `old_tx_hash`.
    pub fn migrate_funding_tx(&mut self, old_tx_hash: &packed::Byte32, new_tx: &FundingTx) {
        if self.map.remove(old_tx_hash).is_some() {
            self.add_funding_tx(new_tx);
        }
    }

    pub fn apply(
        &self,
        collector: &mut dyn CellCollector,
    ) -> Result<(), ckb_sdk::traits::CellCollectorError> {
        for exclusion in self.map.values() {
            for out_point in exclusion.input_out_points.iter() {
                // Cell collector does not need to clean up the locked cells for us
                collector.lock_cell(out_point.clone(), u64::MAX)?;
            }
        }
        Ok(())
    }
}

impl From<TransactionView> for FundingTx {
    fn from(tx: TransactionView) -> Self {
        Self { tx: Some(tx) }
    }
}

impl From<Transaction> for FundingTx {
    fn from(tx: Transaction) -> Self {
        Self {
            tx: Some(tx.into_view()),
        }
    }
}

#[serde_as]
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FundingRequest {
    /// The funding cell lock script args
    #[serde_as(as = "EntityHex")]
    pub script: Script,
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<packed::Script>,
    /// Assets amount to be provided by the local party
    pub local_amount: u128,
    /// Fee to be provided by the local party
    pub funding_fee_rate: u64,
    /// Assets amount to be provided by the remote party
    pub remote_amount: u128,
    /// CKB amount to be provided by the local party.
    pub local_reserved_ckb_amount: u64,
    /// CKB amount to be provided by the remote party.
    pub remote_reserved_ckb_amount: u64,
}

// TODO: trace locked cells
#[derive(Clone, Debug)]
pub struct FundingContext {
    pub rpc_url: String,
    pub funding_source_lock_script: packed::Script,
    pub funding_source_lock_script_cell_deps: Vec<packed::CellDep>,
    pub funding_cell_lock_script: packed::Script,
    pub funding_udt_type_script: Option<packed::Script>,
}

struct ExternalFundingCellDepResolver {
    default: DefaultCellDepResolver,
    funding_source_lock_script: packed::Script,
    funding_source_lock_script_cell_dep: Option<packed::CellDep>,
}

impl CellDepResolver for ExternalFundingCellDepResolver {
    fn resolve(&self, script: &Script) -> Option<packed::CellDep> {
        if script == &self.funding_source_lock_script {
            if let Some(cell_dep) = &self.funding_source_lock_script_cell_dep {
                return Some(cell_dep.clone());
            }
        }
        self.default.resolve(script)
    }
}

#[allow(dead_code)]
pub(crate) struct FundingTxBuilder {
    pub(crate) funding_tx: FundingTx,
    pub(crate) request: FundingRequest,
    pub(crate) context: FundingContext,
}

#[async_trait::async_trait]
impl TxBuilder for FundingTxBuilder {
    async fn build_base_async(
        &self,
        cell_collector: &mut dyn CellCollector,
        _cell_dep_resolver: &dyn CellDepResolver,
        _header_dep_resolver: &dyn HeaderDepResolver,
        _tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, TxBuilderError> {
        let (funding_cell_output, funding_cell_output_data) = self
            .build_funding_cell()
            .map_err(|err| TxBuilderError::Other(err.into()))?;

        self.build_base_from_funding_cell(
            funding_cell_output,
            funding_cell_output_data,
            cell_collector,
        )
        .await
    }
}

impl FundingTxBuilder {
    pub(crate) fn build_funding_cell(
        &self,
    ) -> Result<(packed::CellOutput, packed::Bytes), FundingError> {
        let remote_funded = self
            .funding_tx
            .tx
            .as_ref()
            .map(|tx| !tx.outputs().is_empty())
            .unwrap_or(false);
        debug!(
            "Building funding cell: remote_funded={}, has_udt={}, local_amount={}, remote_amount={}, local_reserved_ckb={}, remote_reserved_ckb={}",
            remote_funded,
            self.request.udt_type_script.is_some(),
            self.request.local_amount,
            self.request.remote_amount,
            self.request.local_reserved_ckb_amount,
            self.request.remote_reserved_ckb_amount,
        );

        match self.request.udt_type_script {
            Some(ref udt_type_script) => {
                let mut udt_amount = self.request.local_amount;
                let mut ckb_amount = self.request.local_reserved_ckb_amount;

                if remote_funded {
                    udt_amount = udt_amount
                        .checked_add(self.request.remote_amount)
                        .ok_or_else(|| {
                            debug!(
                                "Overflow combining UDT amounts: local={} + remote={}",
                                self.request.local_amount, self.request.remote_amount
                            );
                            FundingError::OverflowError
                        })?;
                    ckb_amount = ckb_amount
                        .checked_add(self.request.remote_reserved_ckb_amount)
                        .ok_or_else(|| {
                            debug!(
                                "Overflow combining reserved CKB: local={} + remote={}",
                                self.request.local_reserved_ckb_amount,
                                self.request.remote_reserved_ckb_amount
                            );
                            FundingError::OverflowError
                        })?;
                }

                let udt_output = packed::CellOutput::new_builder()
                    .capacity(Capacity::shannons(ckb_amount).pack())
                    .type_(Some(udt_type_script.clone()).pack())
                    .lock(self.context.funding_cell_lock_script.clone())
                    .build();
                let mut data = BytesMut::with_capacity(16);
                data.put(&udt_amount.to_le_bytes()[..]);

                debug!(
                    "Built UDT funding cell: udt_amount={}, ckb_capacity={} shannons",
                    udt_amount, ckb_amount
                );
                Ok((udt_output, data.freeze().pack()))
            }
            None => {
                let local_amount = u64::try_from(self.request.local_amount).map_err(|_| {
                    debug!(
                        "Local amount {} does not fit into u64 for CKB funding",
                        self.request.local_amount
                    );
                    FundingError::OverflowError
                })?;
                let mut ckb_amount = local_amount
                    .checked_add(self.request.local_reserved_ckb_amount)
                    .ok_or_else(|| {
                        debug!(
                            "Overflow adding local CKB amount {} + reserved {}",
                            local_amount, self.request.local_reserved_ckb_amount
                        );
                        FundingError::OverflowError
                    })?;

                if remote_funded {
                    let remote_amount =
                        u64::try_from(self.request.remote_amount).map_err(|_| {
                            debug!(
                                "Remote amount {} does not fit into u64 for CKB funding",
                                self.request.remote_amount
                            );
                            FundingError::OverflowError
                        })?;
                    ckb_amount = ckb_amount
                        .checked_add(remote_amount)
                        .and_then(|amount| {
                            amount.checked_add(self.request.remote_reserved_ckb_amount)
                        })
                        .ok_or_else(|| {
                            debug!(
                                "Overflow combining CKB amounts: local_total={} + remote={} + remote_reserved={}",
                                ckb_amount,
                                remote_amount,
                                self.request.remote_reserved_ckb_amount
                            );
                            FundingError::OverflowError
                        })?;
                }

                debug!(
                    "Built CKB funding cell with capacity {} shannons",
                    ckb_amount
                );
                let ckb_output = packed::CellOutput::new_builder()
                    .capacity(Capacity::shannons(ckb_amount).pack())
                    .lock(self.context.funding_cell_lock_script.clone())
                    .build();
                Ok((ckb_output, packed::Bytes::default()))
            }
        }
    }

    async fn build_udt_inputs_outputs(
        &self,
        cell_collector: &mut dyn CellCollector,
        inputs: &mut Vec<CellInput>,
        outputs: &mut Vec<packed::CellOutput>,
        outputs_data: &mut Vec<packed::Bytes>,
        cell_deps: &mut HashSet<packed::CellDep>,
    ) -> Result<(), TxBuilderError> {
        let udt_amount = self.request.local_amount;
        if self.request.udt_type_script.is_none() || udt_amount == 0 {
            trace!(
                "Skipping UDT input collection: has_udt_type_script={}, udt_amount={}",
                self.request.udt_type_script.is_some(),
                udt_amount
            );
            return Ok(());
        }

        let udt_type_script = self.request.udt_type_script.clone().ok_or_else(|| {
            TxBuilderError::InvalidParameter(anyhow!("UDT type script not configured"))
        })?;
        let owner = self.context.funding_source_lock_script.clone();
        let mut found_udt_amount: u128 = 0;
        debug!(
            "Collecting UDT cells: target_udt_amount={}, owner_lock_hash={}, udt_type_hash={}",
            udt_amount,
            owner.calc_script_hash(),
            udt_type_script.calc_script_hash(),
        );

        let mut query = CellQueryOptions::new_lock(owner.clone());
        query.script_search_mode = Some(SearchMode::Exact);
        query.secondary_script = Some(udt_type_script.clone());
        query.data_len_range = Some(ValueRangeOption::new_min(16));

        loop {
            let (udt_cells, _) = cell_collector
                .collect_live_cells_async(&query, true)
                .await?;
            if udt_cells.is_empty() {
                trace!(
                    "UDT cell collector returned no more cells; found_udt_amount={}, target={}",
                    found_udt_amount,
                    udt_amount
                );
                break;
            }
            trace!(
                "UDT cell collector returned {} cell(s) (found so far: {}/{})",
                udt_cells.len(),
                found_udt_amount,
                udt_amount
            );
            for cell in udt_cells.iter() {
                let mut amount_bytes = [0u8; 16];
                amount_bytes.copy_from_slice(&cell.output_data.as_ref()[0..16]);
                let cell_udt_amount = u128::from_le_bytes(amount_bytes);
                let ckb_amount: u64 = cell.output.capacity().unpack();
                debug!(
                    "found udt cell ckb_amount: {:?} udt_amount: {:?} cell: {:?}",
                    ckb_amount, cell_udt_amount, cell
                );

                found_udt_amount = found_udt_amount
                    .checked_add(cell_udt_amount)
                    .ok_or_else(|| TxBuilderError::Other(anyhow!("UDT amount overflow")))?;

                inputs.push(CellInput::new(cell.out_point.clone(), 0));

                if found_udt_amount >= udt_amount {
                    let change_amount = found_udt_amount
                        .checked_sub(udt_amount)
                        .ok_or_else(|| TxBuilderError::Other(anyhow!("UDT amount underflow")))?;
                    if change_amount > 0 {
                        let change_output_data: Bytes = change_amount.to_le_bytes().pack();
                        let dummy_output = CellOutput::new_builder()
                            .lock(owner)
                            .type_(Some(udt_type_script.clone()).pack())
                            .build();
                        let required_capacity = dummy_output
                            .occupied_capacity(
                                Capacity::bytes(change_output_data.len())
                                    .map_err(|err| TxBuilderError::Other(err.into()))?,
                            )
                            .map_err(|err| TxBuilderError::Other(err.into()))?
                            .pack();
                        let change_output = dummy_output
                            .as_builder()
                            .capacity(required_capacity)
                            .build();

                        outputs.push(change_output);
                        outputs_data.push(change_output_data);
                    }

                    debug!(
                        "Collected sufficient UDT cells: inputs={}, found_udt_amount={}, change_amount={}",
                            inputs.len(),
                            found_udt_amount,
                            change_amount,
                        );
                    debug!("find proper UDT owner cells: {:?}", inputs);
                    let udt_cell_deps = get_udt_cell_deps(&udt_type_script)
                        .await
                        .map_err(|_| TxBuilderError::ResolveCellDepFailed(udt_type_script))?;
                    for cell_dep in udt_cell_deps {
                        cell_deps.insert(cell_dep);
                    }
                    return Ok(());
                }
            }
        }
        debug!(
            "Insufficient UDT cells: needed {}, found {}",
            udt_amount, found_udt_amount
        );
        Err(TxBuilderError::Other(anyhow!(INSUFFICIENT_UDT_CELLS_MSG)))
    }

    async fn build_base_from_funding_cell(
        &self,
        funding_cell_output: packed::CellOutput,
        funding_cell_output_data: packed::Bytes,
        cell_collector: &mut dyn CellCollector,
    ) -> Result<TransactionView, TxBuilderError> {
        let mut inputs = vec![];
        let mut cell_deps = HashSet::new();
        let mut outputs: Vec<packed::CellOutput> = vec![funding_cell_output];
        let mut outputs_data: Vec<packed::Bytes> = vec![funding_cell_output_data];

        if let Some(ref tx) = self.funding_tx.tx {
            inputs = tx.inputs().into_iter().collect();
            cell_deps = tx.cell_deps().into_iter().collect();
        }
        self.build_udt_inputs_outputs(
            cell_collector,
            &mut inputs,
            &mut outputs,
            &mut outputs_data,
            &mut cell_deps,
        )
        .await?;
        if let Some(ref tx) = self.funding_tx.tx {
            for (i, output) in tx.outputs().into_iter().enumerate().skip(1) {
                outputs.push(output.clone());
                outputs_data.push(tx.outputs_data().get(i).unwrap_or_default().clone());
            }
        }

        let builder = match self.funding_tx.tx {
            Some(ref tx) => tx.as_advanced_builder(),
            None => packed::Transaction::default().as_advanced_builder(),
        };

        let tx_builder = builder
            .set_inputs(inputs)
            .set_outputs(outputs)
            .set_outputs_data(outputs_data)
            .set_cell_deps(cell_deps.into_iter().collect())
            .set_witnesses(vec![secp_sighash_placeholder_witness().as_bytes().pack()]);
        let built = tx_builder.build();
        debug!(
            "Assembled base funding tx: inputs={}, outputs={}, cell_deps={}",
            built.inputs().len(),
            built.outputs().len(),
            built.cell_deps().len(),
        );
        Ok(built)
    }

    async fn build_and_balance_tx(
        &self,
        live_cells_exclusion_map: &mut LiveCellsExclusionMap,
    ) -> Result<TransactionView, FundingError> {
        debug!(
            "Balancing funding tx: funding_source_lock_hash={}, fee_rate={}",
            self.context.funding_source_lock_script.calc_script_hash(),
            self.request.funding_fee_rate,
        );
        let signer = SecpCkbRawKeySigner::new_with_secret_keys(vec![]);
        let sighash_unlocker = SecpSighashUnlocker::from(Box::new(signer) as Box<_>);
        let sighash_script_id = ScriptId::new_type(SIGHASH_TYPE_HASH.clone());
        let mut unlockers = HashMap::default();
        unlockers.insert(
            sighash_script_id,
            Box::new(sighash_unlocker) as Box<dyn ScriptUnlocker>,
        );

        let sender = self.context.funding_source_lock_script.clone();
        let balancer = CapacityBalancer::new_simple(
            sender.clone(),
            secp_sighash_placeholder_witness(),
            self.request.funding_fee_rate,
        );

        let ckb_client = new_ckb_rpc_async_client(&self.context.rpc_url);
        let cell_dep_resolver = match ckb_client.get_block_by_number(0.into()).await? {
            Some(genesis_block) => {
                match DefaultCellDepResolver::from_genesis_async(&BlockView::from(genesis_block))
                    .await
                    .ok()
                {
                    Some(ret) => ret,
                    None => {
                        return Err(FundingError::CkbTxBuilderError(
                            TxBuilderError::ResolveCellDepFailed(sender),
                        ))
                    }
                }
            }
            None => {
                return Err(FundingError::CkbTxBuilderError(
                    TxBuilderError::ResolveCellDepFailed(sender),
                ))
            }
        };
        let cell_dep_resolver = ExternalFundingCellDepResolver {
            default: cell_dep_resolver,
            funding_source_lock_script: self.context.funding_source_lock_script.clone(),
            funding_source_lock_script_cell_dep: self
                .context
                .funding_source_lock_script_cell_deps
                .first()
                .cloned(),
        };

        let header_dep_resolver = DefaultHeaderDepResolver::new(&self.context.rpc_url);
        let mut cell_collector = new_default_cell_collector(&self.context.rpc_url);
        let tx_dep_provider = DefaultTransactionDependencyProvider::new(&self.context.rpc_url, 10);

        let tip_block_number: u64 = ckb_client.get_tip_block_number().await?.into();
        trace!(
            "Funding tx balancing: tip_block_number={}, exclusion_map_size={}",
            tip_block_number,
            live_cells_exclusion_map.map.len(),
        );
        live_cells_exclusion_map.truncate(tip_block_number);
        live_cells_exclusion_map
            .apply(&mut cell_collector)
            .map_err(|err| FundingError::CkbTxBuilderError(TxBuilderError::Other(err.into())))?;

        #[cfg(not(target_arch = "wasm32"))]
        let build_result = self.build_unlocked(
            &mut cell_collector,
            &cell_dep_resolver,
            &header_dep_resolver,
            &tx_dep_provider,
            &balancer,
            &unlockers,
        );
        #[cfg(target_arch = "wasm32")]
        let build_result = self
            .build_unlocked_async(
                &mut cell_collector,
                &cell_dep_resolver,
                &header_dep_resolver,
                &tx_dep_provider,
                &balancer,
                &unlockers,
            )
            .await;
        let (tx, _) = build_result.map_err(|err| {
            debug!("Funding tx capacity balancing failed: {:?}", err);
            map_tx_builder_error(err)
        })?;

        debug!(
            "Funding tx balanced: tx_hash={}, inputs={}, outputs={}",
            tx.hash(),
            tx.inputs().len(),
            tx.outputs().len(),
        );
        Ok(tx)
    }

    fn with_extra_cell_deps(&self, tx: TransactionView) -> TransactionView {
        if self.context.funding_source_lock_script_cell_deps.is_empty() {
            return tx;
        }

        let mut cell_deps: Vec<_> = tx.cell_deps_iter().collect();
        for cell_dep in &self.context.funding_source_lock_script_cell_deps {
            if !cell_deps.contains(cell_dep) {
                cell_deps.push(cell_dep.clone());
            }
        }

        tx.as_advanced_builder().set_cell_deps(cell_deps).build()
    }
    fn finalize_funding_tx_update(
        self,
        tx: TransactionView,
        live_cells_exclusion_map: &mut LiveCellsExclusionMap,
    ) -> FundingTx {
        let old_tx_hash = self
            .funding_tx
            .tx
            .as_ref()
            .map(|current_tx| current_tx.hash());
        let mut funding_tx = self.funding_tx;
        funding_tx.update_for_self(tx);
        if let Some(tx_hash) = old_tx_hash {
            live_cells_exclusion_map.remove(&tx_hash);
        }
        live_cells_exclusion_map.add_funding_tx(&funding_tx);
        funding_tx
    }

    async fn build(
        self,
        live_cells_exclusion_map: &mut LiveCellsExclusionMap,
    ) -> Result<FundingTx, FundingError> {
        debug!(
            "FundingTxBuilder::build starting: had_existing_tx={}, fee_rate={}",
            self.funding_tx.tx.is_some(),
            self.request.funding_fee_rate,
        );
        let tx = self.build_and_balance_tx(live_cells_exclusion_map).await?;
        let tx = self.with_extra_cell_deps(tx);
        debug!("final tx_builder: {:?}", tx.as_advanced_builder());
        Ok(self.finalize_funding_tx_update(tx, live_cells_exclusion_map))
    }
}

impl FundingTx {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take(&mut self) -> Option<TransactionView> {
        self.tx.take()
    }

    pub fn as_ref(&self) -> Option<&TransactionView> {
        self.tx.as_ref()
    }

    pub fn into_inner(self) -> Option<TransactionView> {
        self.tx
    }

    pub async fn fulfill(
        self,
        request: FundingRequest,
        context: FundingContext,
        live_cells_exclusion_map: &mut LiveCellsExclusionMap,
    ) -> Result<Self, FundingError> {
        let builder = FundingTxBuilder {
            funding_tx: self,
            request,
            context,
        };
        builder.build(live_cells_exclusion_map).await
    }

    /// Build an unsigned funding transaction for external signing.
    ///
    /// This method collects cells from the user's wallet (identified by
    /// `funding_source_lock_script`) and builds a final, balanced transaction
    /// with proper inputs and outputs, but does NOT sign it. The user is
    /// expected to sign this transaction externally with their own wallet and
    /// submit it directly without changing transaction structure.
    pub async fn build_unsigned_for_external_funding(
        self,
        request: FundingRequest,
        context: FundingContext,
        live_cells_exclusion_map: &mut LiveCellsExclusionMap,
    ) -> Result<Self, FundingError> {
        self.fulfill(request, context, live_cells_exclusion_map)
            .await
    }

    pub async fn sign(
        mut self,
        secret_key: secp256k1::SecretKey,
        rpc_url: String,
    ) -> Result<Self, FundingError> {
        // Convert between different versions of secp256k1.
        // This app requires 0.28 because of:
        // ```
        // #[derive(Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
        // pub struct Signature(pub Secp256k1Signature);
        // ```
        //
        // However, ckb-sdk-rust still uses 0.24.
        //
        // It's complex to use map_err and return an error as well because secp256k1 used by ckb sdk is not public.
        // Expect is OK here since the secret key is valid and can be parsed in both versions.
        let signer = SecpCkbRawKeySigner::new_with_secret_keys(vec![std::str::FromStr::from_str(
            hex::encode(secret_key.as_ref()).as_ref(),
        )
        .expect("convert secret key between different secp256k1 versions")]);
        let sighash_unlocker = SecpSighashUnlocker::from(Box::new(signer) as Box<_>);
        let sighash_script_id = ScriptId::new_type(SIGHASH_TYPE_HASH.clone());
        let mut unlockers = HashMap::default();
        unlockers.insert(
            sighash_script_id,
            Box::new(sighash_unlocker) as Box<dyn ScriptUnlocker>,
        );
        let tx = self.take().ok_or(FundingError::AbsentTx)?;
        let tx_hash_before = tx.hash();
        debug!("Signing funding tx: tx_hash={}", tx_hash_before);
        let tx_dep_provider = DefaultTransactionDependencyProvider::new(&rpc_url, 10);

        let (tx, _) = unlock_tx_async(tx, &tx_dep_provider, &unlockers)
            .await
            .map_err(|err| {
                debug!("Failed to sign funding tx {}: {:?}", tx_hash_before, err);
                err
            })?;
        debug!(
            "Signed funding tx: tx_hash={}, witnesses={}",
            tx.hash(),
            tx.witnesses().len()
        );
        self.update_for_self(tx);
        Ok(self)
    }

    pub fn update_for_self(&mut self, tx: TransactionView) {
        self.tx = Some(tx);
    }

    /// Verify the funding tx received from the peer via the message TxUpdate.
    ///
    /// Replace the funding tx on success, otherwise return the error and leave the funding tx
    /// untouched.
    ///
    /// The received remote funding tx is verified against the local version. It is assumed that
    /// the peer only performs necessary operations to fund the channel.
    pub async fn update_for_peer(
        &mut self,
        remote_tx: TransactionView,
        context: FundingContext,
    ) -> Result<(), FundingError> {
        let local_tx = self
            .tx
            .clone()
            .unwrap_or_else(|| Transaction::default().into_view());
        debug!(
            "Verifying peer funding tx update: local_tx_hash={}, remote_tx_hash={}, local_inputs={}, remote_inputs={}, local_outputs={}, remote_outputs={}",
            local_tx.hash(),
            remote_tx.hash(),
            local_tx.inputs().len(),
            remote_tx.inputs().len(),
            local_tx.outputs().len(),
            remote_tx.outputs().len(),
        );

        // Bound the peer-added complexity before any chain lookup, so a single
        // TxUpdate cannot amplify into an unbounded number of CKB RPC requests.
        validate_peer_funding_tx_complexity(&local_tx, &remote_tx)?;

        // Version MUST be the same
        if remote_tx.version() != local_tx.version() {
            debug!(
                "invalid remote funding tx (version): remote={} local={}",
                remote_tx.version(),
                local_tx.version()
            );
            return Err(FundingError::InvalidPeerFundingTx);
        }

        // Peer SHOULD NOT remove cell_deps
        if remote_tx.cell_deps().len() < local_tx.cell_deps().len() {
            debug!(
                "invalid remote funding tx (cell_deps.len): remote={} local={}",
                remote_tx.cell_deps().len(),
                local_tx.cell_deps().len(),
            );
            return Err(FundingError::InvalidPeerFundingTx);
        }
        let cell_deps_set: HashSet<_> = remote_tx.cell_deps_iter().collect();
        for cell_dep in local_tx.cell_deps_iter() {
            if !cell_deps_set.contains(&cell_dep) {
                debug!(
                    "invalid remote funding tx (cell_deps), cell dep missing in the remote version: {:?}",
                    cell_dep
                );
                return Err(FundingError::InvalidPeerFundingTx);
            }
        }

        // Peer SHOULD NOT remove header_deps
        if remote_tx.header_deps().len() < local_tx.header_deps().len() {
            debug!(
                "invalid remote funding tx (header_deps.len): remote={} local={}",
                remote_tx.header_deps().len(),
                local_tx.header_deps().len(),
            );
            return Err(FundingError::InvalidPeerFundingTx);
        }
        if remote_tx
            .header_deps()
            .into_iter()
            .zip(local_tx.header_deps())
            .any(|(remote, local)| remote != local)
        {
            debug!("invalid remote funding tx (header_deps)");
            return Err(FundingError::InvalidPeerFundingTx);
        }

        // Peer SHOULD NOT remove inputs
        if remote_tx.inputs().len() < local_tx.inputs().len() {
            debug!(
                "invalid remote funding tx (inputs.len): remote={} local={}",
                remote_tx.inputs().len(),
                local_tx.inputs().len(),
            );
            return Err(FundingError::InvalidPeerFundingTx);
        }
        // Peer SHOULD NOT modify existing inputs
        if remote_tx
            .inputs()
            .into_iter()
            .zip(local_tx.inputs())
            .any(|(remote, local)| remote != local)
        {
            debug!("invalid funding tx (inputs)");
            return Err(FundingError::InvalidPeerFundingTx);
        }

        // Peer SHOULD NOT remove outputs
        if remote_tx.outputs().len() < local_tx.outputs().len() {
            debug!(
                "invalid funding tx (outputs.len): remote={} local={}",
                remote_tx.outputs().len(),
                local_tx.outputs().len(),
            );
            return Err(FundingError::InvalidPeerFundingTx);
        }
        // The first output MUST be the funding cell
        if let Some(output) = remote_tx.output(0) {
            if output.lock() != context.funding_cell_lock_script {
                debug!("invalid funding tx (outputs[0]): not a funding cell",);
                return Err(FundingError::InvalidPeerFundingTx);
            }
            let output_type_opt: Option<packed::Script> = output.type_().to_opt();
            if let Some(ref expected_udt_script) = context.funding_udt_type_script {
                if output_type_opt.as_ref() != Some(expected_udt_script) {
                    debug!(
                        "invalid funding tx (outputs[0]): UDT type script mismatch, expected {:?}",
                        expected_udt_script
                    );
                    return Err(FundingError::InvalidPeerFundingTx);
                }
            } else if output_type_opt.is_some() {
                debug!("invalid funding tx (outputs[0]): unexpected type script for CKB channel");
                return Err(FundingError::InvalidPeerFundingTx);
            }
            if let Some(data) = remote_tx.outputs_data().get(0) {
                if output_type_opt.is_none() && !data.is_empty() {
                    debug!(
                        "invalid funding tx (outputs_data[0]): data is not allowed for CKB channel",
                    );
                    return Err(FundingError::InvalidPeerFundingTx);
                }
            }
        }
        // Peer SHOULD NOT modify existing outputs
        if let Some((index, (remote_output, local_output))) = remote_tx
            .outputs()
            .into_iter()
            .zip(local_tx.outputs())
            .enumerate()
            .skip(1)
            .find(|(_, (remote, local))| remote != local)
        {
            if tracing::enabled!(tracing::Level::DEBUG) {
                let remote_output: ckb_jsonrpc_types::CellOutput = remote_output.into();
                let local_output: ckb_jsonrpc_types::CellOutput = local_output.into();
                debug!(
                    "invalid funding tx (outputs[{}]): remote={:?} local={:?}",
                    index, remote_output, local_output
                );
            }
            return Err(FundingError::InvalidPeerFundingTx);
        }

        // Peer SHOULD NOT modify existing outputs data
        if remote_tx.outputs_data().len() < local_tx.outputs_data().len() {
            debug!(
                "invalid funding tx (outputs_data.len): remote={} local={}",
                remote_tx.outputs_data().len(),
                local_tx.outputs_data().len(),
            );
            return Err(FundingError::InvalidPeerFundingTx);
        }
        // Peer CAN only modify the funding cell.
        if remote_tx
            .outputs_data()
            .into_iter()
            .zip(local_tx.outputs_data())
            .skip(1)
            .any(|(remote, local)| remote != local)
        {
            debug!("invalid funding tx (outputs_data)");
            return Err(FundingError::InvalidPeerFundingTx);
        }

        // Ignore witnesses

        // Peer SHOULD NOT add inputs locked by our lock scripts. This requires one
        // get_live_cell RPC per peer-added input, so it runs last, after all the
        // cheap structural checks above (including the complexity budget) have
        // passed.
        let ckb_client = new_ckb_rpc_async_client(&context.rpc_url);
        for (input_index, input) in remote_tx
            .input_pts_iter()
            .enumerate()
            .skip(local_tx.inputs().len())
        {
            match ckb_client.get_live_cell(input.into(), false).await?.cell {
                Some(cell) => {
                    let cell_output_lock: packed::Script = cell.output.lock.into();
                    if cell_output_lock == context.funding_source_lock_script {
                        debug!(
                            "invalid funding tx (inputs): peer uses input #{} with our lock script",
                            input_index
                        );
                        return Err(FundingError::PeerInputUsesOurFundingLock { input_index });
                    }
                }
                None => {
                    debug!("invalid funding tx (inputs): dead input");
                    return Err(FundingError::InvalidPeerFundingTx);
                }
            };
        }

        self.tx = Some(remote_tx);
        Ok(())
    }
}
