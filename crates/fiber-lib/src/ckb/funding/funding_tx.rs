use super::super::{signer::LocalSigner, FundingError};
use crate::{
    ckb::{
        config::{new_ckb_rpc_async_client, new_default_cell_collector},
        contracts::get_udt_cell_deps,
    },
    fiber::serde_utils::EntityHex,
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
use molecule::{
    bytes::{BufMut as _, BytesMut},
    prelude::*,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::{HashMap, HashSet};
use tracing::debug;

// Number of blocks to keep the committed funding tx in the exclusion map.
// It is the same with the value used in the CKB SDK.
const KEEP_BLOCK_PERIOD: u64 = 13;

/// Funding transaction wrapper.
///
/// It includes extra fields to verify the transaction.
#[derive(Clone, Debug, Default)]
pub struct FundingTx {
    tx: Option<TransactionView>,
}

#[derive(Debug, Clone)]
struct LiveCellsExclusion {
    input_out_points: Vec<packed::OutPoint>,
    committed_block_number: Option<u64>,
}

#[derive(Debug, Default, Clone)]
pub struct LiveCellsExclusionMap {
    map: HashMap<packed::Byte32, LiveCellsExclusion>,
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
                committed_block_number + KEEP_BLOCK_PERIOD > tip_block_number
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
    pub signer: LocalSigner,
    pub rpc_url: String,
    pub funding_source_lock_script: packed::Script,
    pub funding_cell_lock_script: packed::Script,
}

/// Context for building unsigned funding transaction for external signing.
/// Unlike FundingContext, this does not include the secret key since the user
/// will sign the transaction externally with their own wallet.
#[derive(Clone, Debug)]
pub struct ExternalFundingContext {
    pub rpc_url: String,
    /// The lock script of the cells to use for funding (user's wallet lock script)
    pub funding_source_lock_script: packed::Script,
    /// The lock script of the funding cell (channel multisig lock)
    pub funding_cell_lock_script: packed::Script,
}

/// Collect UDT inputs and build change outputs for funding transactions.
/// This is shared logic used by both FundingTxBuilder and ExternalFundingTxBuilder.
async fn collect_udt_inputs_outputs(
    request: &FundingRequest,
    funding_source_lock_script: &packed::Script,
    cell_collector: &mut dyn CellCollector,
    inputs: &mut Vec<CellInput>,
    outputs: &mut Vec<packed::CellOutput>,
    outputs_data: &mut Vec<packed::Bytes>,
    cell_deps: &mut HashSet<packed::CellDep>,
) -> Result<(), TxBuilderError> {
    let udt_amount = request.local_amount;
    // return early if we don't need to build UDT cell
    if request.udt_type_script.is_none() || udt_amount == 0 {
        return Ok(());
    }

    let udt_type_script = request.udt_type_script.clone().ok_or_else(|| {
        TxBuilderError::InvalidParameter(anyhow!("UDT type script not configured"))
    })?;
    let owner = funding_source_lock_script.clone();
    let mut found_udt_amount: u128 = 0;

    let mut query = CellQueryOptions::new_lock(owner.clone());
    query.script_search_mode = Some(SearchMode::Exact);
    query.secondary_script = Some(udt_type_script.clone());
    query.data_len_range = Some(ValueRangeOption::new_min(16));

    loop {
        // each query will found at most one cell because of `min_total_capacity == 1` in CellQueryOptions
        let (udt_cells, _) = cell_collector
            .collect_live_cells_async(&query, true)
            .await?;
        if udt_cells.is_empty() {
            break;
        }
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
                let change_amount = found_udt_amount - udt_amount;
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

                debug!("find proper UDT owner cells: {:?}", inputs);
                // we need to filter the cell deps by the contracts_context
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
    Err(TxBuilderError::Other(anyhow!(
        "can not find enough UDT owner cells for funding transaction"
    )))
}

/// Build the funding transaction base from a pre-built funding cell.
/// This is shared logic used by both FundingTxBuilder and ExternalFundingTxBuilder
/// in their TxBuilder::build_base_async implementations.
async fn build_base_from_funding_cell(
    funding_cell_output: packed::CellOutput,
    funding_cell_output_data: packed::Bytes,
    funding_tx: &FundingTx,
    request: &FundingRequest,
    funding_source_lock_script: &packed::Script,
    cell_collector: &mut dyn CellCollector,
) -> Result<TransactionView, TxBuilderError> {
    let mut inputs = vec![];
    let mut cell_deps = HashSet::new();

    // Funding cell does not need new cell deps and header deps. The type script deps will be added with inputs.
    let mut outputs: Vec<packed::CellOutput> = vec![funding_cell_output];
    let mut outputs_data: Vec<packed::Bytes> = vec![funding_cell_output_data];

    if let Some(ref tx) = funding_tx.tx {
        inputs = tx.inputs().into_iter().collect();
        cell_deps = tx.cell_deps().into_iter().collect();
    }
    collect_udt_inputs_outputs(
        request,
        funding_source_lock_script,
        cell_collector,
        &mut inputs,
        &mut outputs,
        &mut outputs_data,
        &mut cell_deps,
    )
    .await?;
    if let Some(ref tx) = funding_tx.tx {
        for (i, output) in tx.outputs().into_iter().enumerate().skip(1) {
            outputs.push(output.clone());
            outputs_data.push(tx.outputs_data().get(i).unwrap_or_default().clone());
        }
    }

    let builder = match funding_tx.tx {
        Some(ref tx) => tx.as_advanced_builder(),
        None => packed::Transaction::default().as_advanced_builder(),
    };

    // set a placeholder_witness for calculating transaction fee according to transaction size
    let placeholder_witness = packed::WitnessArgs::new_builder()
        .lock(Some(molecule::bytes::Bytes::from(vec![0u8; 170])).pack())
        .build();

    let tx_builder = builder
        .set_inputs(inputs)
        .set_outputs(outputs)
        .set_outputs_data(outputs_data)
        .set_cell_deps(cell_deps.into_iter().collect())
        .set_witnesses(vec![placeholder_witness.as_bytes().pack()]);
    let tx = tx_builder.build();
    Ok(tx)
}

/// Balance a funding transaction using CKB RPC, build and unlock it (with placeholder signatures).
/// This is shared logic used by both FundingTxBuilder and ExternalFundingTxBuilder.
async fn build_and_balance_tx<T: TxBuilder>(
    builder: &T,
    rpc_url: &str,
    funding_source_lock_script: &packed::Script,
    funding_fee_rate: u64,
    live_cells_exclusion_map: &mut LiveCellsExclusionMap,
) -> Result<TransactionView, FundingError> {
    // Build ScriptUnlocker
    let signer = SecpCkbRawKeySigner::new_with_secret_keys(vec![]);
    let sighash_unlocker = SecpSighashUnlocker::from(Box::new(signer) as Box<_>);
    let sighash_script_id = ScriptId::new_type(SIGHASH_TYPE_HASH.clone());
    let mut unlockers = HashMap::default();
    unlockers.insert(
        sighash_script_id,
        Box::new(sighash_unlocker) as Box<dyn ScriptUnlocker>,
    );

    let sender = funding_source_lock_script.clone();
    // Build CapacityBalancer
    let placeholder_witness = packed::WitnessArgs::new_builder()
        .lock(Some(molecule::bytes::Bytes::from(vec![0u8; 170])).pack())
        .build();

    let balancer =
        CapacityBalancer::new_simple(sender.clone(), placeholder_witness, funding_fee_rate);

    let ckb_client = new_ckb_rpc_async_client(rpc_url);
    let cell_dep_resolver = {
        match ckb_client.get_block_by_number(0.into()).await? {
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
        }
    };

    let header_dep_resolver = DefaultHeaderDepResolver::new(rpc_url);
    let mut cell_collector = new_default_cell_collector(rpc_url);
    let tx_dep_provider = DefaultTransactionDependencyProvider::new(rpc_url, 10);

    let tip_block_number: u64 = ckb_client.get_tip_block_number().await?.into();
    live_cells_exclusion_map.truncate(tip_block_number);
    live_cells_exclusion_map
        .apply(&mut cell_collector)
        .map_err(|err| FundingError::CkbTxBuilderError(TxBuilderError::Other(err.into())))?;

    #[cfg(not(target_arch = "wasm32"))]
    let (tx, _) = builder.build_unlocked(
        &mut cell_collector,
        &cell_dep_resolver,
        &header_dep_resolver,
        &tx_dep_provider,
        &balancer,
        &unlockers,
    )?;
    #[cfg(target_arch = "wasm32")]
    let (tx, _) = builder
        .build_unlocked_async(
            &mut cell_collector,
            &cell_dep_resolver,
            &header_dep_resolver,
            &tx_dep_provider,
            &balancer,
            &unlockers,
        )
        .await?;

    Ok(tx)
}

/// Finalize the funding transaction by updating FundingTx and the exclusion map.
/// This is shared logic used by both FundingTxBuilder and ExternalFundingTxBuilder.
fn finalize_funding_tx_update(
    funding_tx: FundingTx,
    tx: TransactionView,
    live_cells_exclusion_map: &mut LiveCellsExclusionMap,
) -> FundingTx {
    let old_tx_hash = funding_tx.tx.as_ref().map(|tx| tx.hash());
    let mut funding_tx = funding_tx;
    funding_tx.update_for_self(tx);
    if let Some(tx_hash) = old_tx_hash {
        live_cells_exclusion_map.remove(&tx_hash);
    }
    live_cells_exclusion_map.add_funding_tx(&funding_tx);
    funding_tx
}

#[allow(dead_code)]
struct FundingTxBuilder {
    funding_tx: FundingTx,
    request: FundingRequest,
    context: FundingContext,
}

#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
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

        build_base_from_funding_cell(
            funding_cell_output,
            funding_cell_output_data,
            &self.funding_tx,
            &self.request,
            &self.context.funding_source_lock_script,
            cell_collector,
        )
        .await
    }
}

impl FundingTxBuilder {
    fn build_funding_cell(&self) -> Result<(packed::CellOutput, packed::Bytes), FundingError> {
        // If outputs is not empty, assume that the remote party has already funded.
        let remote_funded = self
            .funding_tx
            .tx
            .as_ref()
            .map(|tx| !tx.outputs().is_empty())
            .unwrap_or(false);

        match self.request.udt_type_script {
            Some(ref udt_type_script) => {
                let mut udt_amount = self.request.local_amount;
                let mut ckb_amount = self.request.local_reserved_ckb_amount;

                // To make tx building easier, do not include the amount not funded yet in the
                // funding cell.
                if remote_funded {
                    udt_amount = udt_amount
                        .checked_add(self.request.remote_amount)
                        .ok_or(FundingError::OverflowError)?;
                    ckb_amount = ckb_amount
                        .checked_add(self.request.remote_reserved_ckb_amount)
                        .ok_or(FundingError::OverflowError)?;
                }

                let udt_output = packed::CellOutput::new_builder()
                    .capacity(Capacity::shannons(ckb_amount).pack())
                    .type_(Some(udt_type_script.clone()).pack())
                    .lock(self.context.funding_cell_lock_script.clone())
                    .build();
                let mut data = BytesMut::with_capacity(16);
                data.put(&udt_amount.to_le_bytes()[..]);

                // TODO: xudt extension
                Ok((udt_output, data.freeze().pack()))
            }
            None => {
                let mut ckb_amount = (self.request.local_amount as u64)
                    .checked_add(self.request.local_reserved_ckb_amount)
                    .ok_or(FundingError::OverflowError)?;
                if remote_funded {
                    ckb_amount = ckb_amount
                        .checked_add(
                            self.request.remote_amount as u64
                                + self.request.remote_reserved_ckb_amount,
                        )
                        .ok_or(FundingError::OverflowError)?;
                }
                let ckb_output = packed::CellOutput::new_builder()
                    .capacity(Capacity::shannons(ckb_amount).pack())
                    .lock(self.context.funding_cell_lock_script.clone())
                    .build();
                Ok((ckb_output, packed::Bytes::default()))
            }
        }
    }

    async fn build(
        self,
        live_cells_exclusion_map: &mut LiveCellsExclusionMap,
    ) -> Result<FundingTx, FundingError> {
        let tx = build_and_balance_tx(
            &self,
            &self.context.rpc_url,
            &self.context.funding_source_lock_script,
            self.request.funding_fee_rate,
            live_cells_exclusion_map,
        )
        .await?;
        debug!("final tx_builder: {:?}", tx.as_advanced_builder());
        Ok(finalize_funding_tx_update(
            self.funding_tx,
            tx,
            live_cells_exclusion_map,
        ))
    }
}

/// Builder for unsigned funding transactions for external signing.
/// This is similar to FundingTxBuilder but does not sign the transaction.
#[allow(dead_code)]
struct ExternalFundingTxBuilder {
    funding_tx: FundingTx,
    request: FundingRequest,
    context: ExternalFundingContext,
}

#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl TxBuilder for ExternalFundingTxBuilder {
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

        build_base_from_funding_cell(
            funding_cell_output,
            funding_cell_output_data,
            &self.funding_tx,
            &self.request,
            &self.context.funding_source_lock_script,
            cell_collector,
        )
        .await
    }
}

impl ExternalFundingTxBuilder {
    fn build_funding_cell(&self) -> Result<(packed::CellOutput, packed::Bytes), FundingError> {
        match self.request.udt_type_script {
            Some(ref udt_type_script) => {
                let udt_amount = self
                    .request
                    .local_amount
                    .checked_add(self.request.remote_amount)
                    .ok_or(FundingError::OverflowError)?;
                let ckb_amount = self
                    .request
                    .local_reserved_ckb_amount
                    .checked_add(self.request.remote_reserved_ckb_amount)
                    .ok_or(FundingError::OverflowError)?;

                let udt_output = packed::CellOutput::new_builder()
                    .capacity(Capacity::shannons(ckb_amount).pack())
                    .type_(Some(udt_type_script.clone()).pack())
                    .lock(self.context.funding_cell_lock_script.clone())
                    .build();
                let mut data = BytesMut::with_capacity(16);
                data.put(&udt_amount.to_le_bytes()[..]);

                // TODO: xudt extension
                Ok((udt_output, data.freeze().pack()))
            }
            None => {
                let local_amount = u64::try_from(self.request.local_amount)
                    .map_err(|_| FundingError::OverflowError)?;
                let remote_amount = u64::try_from(self.request.remote_amount)
                    .map_err(|_| FundingError::OverflowError)?;
                let ckb_amount = local_amount
                    .checked_add(self.request.local_reserved_ckb_amount)
                    .and_then(|amount| amount.checked_add(remote_amount))
                    .and_then(|amount| amount.checked_add(self.request.remote_reserved_ckb_amount))
                    .ok_or(FundingError::OverflowError)?;
                let ckb_output = packed::CellOutput::new_builder()
                    .capacity(Capacity::shannons(ckb_amount).pack())
                    .lock(self.context.funding_cell_lock_script.clone())
                    .build();
                Ok((ckb_output, packed::Bytes::default()))
            }
        }
    }

    /// Build an unsigned funding transaction for external signing.
    /// This collects cells, balances capacity, but does NOT sign the transaction.
    async fn build(
        self,
        live_cells_exclusion_map: &mut LiveCellsExclusionMap,
    ) -> Result<FundingTx, FundingError> {
        let tx = build_and_balance_tx(
            &self,
            &self.context.rpc_url,
            &self.context.funding_source_lock_script,
            self.request.funding_fee_rate,
            live_cells_exclusion_map,
        )
        .await?;
        debug!("built unsigned funding tx: {:?}", tx);
        Ok(finalize_funding_tx_update(
            self.funding_tx,
            tx,
            live_cells_exclusion_map,
        ))
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
        context: ExternalFundingContext,
        live_cells_exclusion_map: &mut LiveCellsExclusionMap,
    ) -> Result<Self, FundingError> {
        let builder = ExternalFundingTxBuilder {
            funding_tx: self,
            request,
            context,
        };
        builder.build(live_cells_exclusion_map).await
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
        let tx_dep_provider = DefaultTransactionDependencyProvider::new(&rpc_url, 10);

        let (tx, _) = unlock_tx_async(tx, &tx_dep_provider, &unlockers).await?;
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
        // Peer SHOULD NOT add inputs locked by our lock scripts
        let ckb_client = new_ckb_rpc_async_client(&context.rpc_url);
        for input in remote_tx.input_pts_iter().skip(local_tx.inputs().len()) {
            match ckb_client.get_live_cell(input.into(), false).await?.cell {
                Some(cell) => {
                    let cell_output_lock: packed::Script = cell.output.lock.into();
                    if cell_output_lock == context.funding_source_lock_script {
                        debug!(
                            "invalid funding tx (inputs): peer uses inputs with our lock script"
                        );
                        return Err(FundingError::InvalidPeerFundingTx);
                    }
                }
                None => {
                    debug!("invalid funding tx (inputs): dead input");
                    return Err(FundingError::InvalidPeerFundingTx);
                }
            };
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
            if let Some(data) = remote_tx.outputs_data().get(0) {
                if output.type_().is_none() && !data.is_empty() {
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

        self.tx = Some(remote_tx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a dummy ExternalFundingContext for unit tests.
    fn dummy_external_context() -> ExternalFundingContext {
        let script = Script::default();
        ExternalFundingContext {
            rpc_url: String::new(),
            funding_source_lock_script: script.clone(),
            funding_cell_lock_script: script,
        }
    }

    #[test]
    fn test_external_funding_build_ckb_funding_cell() {
        let context = dummy_external_context();
        let request = FundingRequest {
            script: Script::default(),
            udt_type_script: None,
            local_amount: 100_000_000_000, // 1000 CKB
            remote_amount: 50_000_000_000,
            funding_fee_rate: 1000,
            local_reserved_ckb_amount: 6_200_000_000,
            remote_reserved_ckb_amount: 6_200_000_000,
        };

        let builder = ExternalFundingTxBuilder {
            funding_tx: FundingTx::new(),
            request: request.clone(),
            context: context.clone(),
        };

        let (output, data) = builder.build_funding_cell().expect("build funding cell");

        // For CKB channels: capacity = local_amount + remote_amount + local_reserved + remote_reserved
        let expected_capacity: u64 = request.local_amount as u64
            + request.remote_amount as u64
            + request.local_reserved_ckb_amount
            + request.remote_reserved_ckb_amount;
        let actual_capacity: u64 = output.capacity().unpack();
        assert_eq!(actual_capacity, expected_capacity);

        // Lock script should be the funding cell lock script
        assert_eq!(output.lock(), context.funding_cell_lock_script);

        // No type script for CKB channels
        assert!(output.type_().is_none());

        // Data should be empty for CKB channels
        assert_eq!(data, packed::Bytes::default());
    }

    #[test]
    fn test_external_funding_build_udt_funding_cell() {
        let context = dummy_external_context();
        let udt_type_script = Script::new_builder()
            .code_hash(packed::Byte32::from_slice(&[1u8; 32]).unwrap())
            .hash_type(packed::Byte::new(0))
            .build();

        let request = FundingRequest {
            script: Script::default(),
            udt_type_script: Some(udt_type_script.clone()),
            local_amount: 1_000_000, // UDT amount
            remote_amount: 500_000,
            funding_fee_rate: 1000,
            local_reserved_ckb_amount: 14_200_000_000,
            remote_reserved_ckb_amount: 14_200_000_000,
        };

        let builder = ExternalFundingTxBuilder {
            funding_tx: FundingTx::new(),
            request: request.clone(),
            context: context.clone(),
        };

        let (output, data) = builder.build_funding_cell().expect("build funding cell");

        // For UDT channels: capacity = local_reserved + remote_reserved
        let expected_capacity =
            request.local_reserved_ckb_amount + request.remote_reserved_ckb_amount;
        let actual_capacity: u64 = output.capacity().unpack();
        assert_eq!(actual_capacity, expected_capacity);

        // Lock script should be the funding cell lock script
        assert_eq!(output.lock(), context.funding_cell_lock_script);

        // Type script should be the UDT type script
        assert_eq!(
            output.type_().to_opt().expect("has type script"),
            udt_type_script
        );

        // Data should contain the total UDT amount (16 bytes, little-endian)
        let expected_udt_amount: u128 = request.local_amount + request.remote_amount;
        let data_bytes = data.raw_data();
        assert_eq!(data_bytes.len(), 16);
        let mut amount_bytes = [0u8; 16];
        amount_bytes.copy_from_slice(&data_bytes[..16]);
        assert_eq!(u128::from_le_bytes(amount_bytes), expected_udt_amount);
    }

    #[test]
    fn test_external_funding_build_funding_cell_overflow() {
        let context = dummy_external_context();

        // Create a request where amounts overflow u64
        let request = FundingRequest {
            script: Script::default(),
            udt_type_script: None,
            local_amount: u64::MAX as u128,
            remote_amount: u64::MAX as u128,
            funding_fee_rate: 1000,
            local_reserved_ckb_amount: u64::MAX,
            remote_reserved_ckb_amount: u64::MAX,
        };

        let builder = ExternalFundingTxBuilder {
            funding_tx: FundingTx::new(),
            request,
            context,
        };

        let result = builder.build_funding_cell();
        assert!(result.is_err(), "should overflow");
        match result {
            Err(FundingError::OverflowError) => {} // expected
            other => panic!("expected OverflowError, got {:?}", other),
        }
    }

    #[test]
    fn test_external_funding_build_udt_funding_cell_overflow() {
        let context = dummy_external_context();
        let udt_type_script = Script::new_builder()
            .code_hash(packed::Byte32::from_slice(&[1u8; 32]).unwrap())
            .hash_type(packed::Byte::new(0))
            .build();

        // UDT amounts that overflow u128
        let request = FundingRequest {
            script: Script::default(),
            udt_type_script: Some(udt_type_script),
            local_amount: u128::MAX,
            remote_amount: 1,
            funding_fee_rate: 1000,
            local_reserved_ckb_amount: 14_200_000_000,
            remote_reserved_ckb_amount: 14_200_000_000,
        };

        let builder = ExternalFundingTxBuilder {
            funding_tx: FundingTx::new(),
            request,
            context,
        };

        let result = builder.build_funding_cell();
        assert!(result.is_err(), "should overflow");
        match result {
            Err(FundingError::OverflowError) => {} // expected
            other => panic!("expected OverflowError, got {:?}", other),
        }
    }

    #[test]
    fn test_external_funding_build_ckb_funding_cell_amount_cast_overflow() {
        let context = dummy_external_context();
        let request = FundingRequest {
            script: Script::default(),
            udt_type_script: None,
            local_amount: (u64::MAX as u128) + 1,
            remote_amount: 0,
            funding_fee_rate: 1000,
            local_reserved_ckb_amount: 0,
            remote_reserved_ckb_amount: 0,
        };

        let builder = ExternalFundingTxBuilder {
            funding_tx: FundingTx::new(),
            request,
            context,
        };

        let result = builder.build_funding_cell();
        match result {
            Err(FundingError::OverflowError) => {}
            other => panic!("expected OverflowError, got {:?}", other),
        }
    }
}
