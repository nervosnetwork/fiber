use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::anyhow;
use ckb_hash::new_blake2b;
use ckb_jsonrpc_types::{Either, Status};
use ckb_sdk::{
    rpc::ckb_indexer::{Cell, Order, ScriptType, SearchKey, SearchKeyFilter, SearchMode},
    traits::{CellCollector, CellQueryOptions, DefaultCellCollector, ValueRangeOption},
    transaction::builder::FeeCalculator,
    util::blake160,
    CkbRpcClient, RpcError, Since, SinceType,
};
use ckb_types::{
    self,
    core::{Capacity, EpochNumberWithFraction, HeaderView, TransactionBuilder, TransactionView},
    packed::{Bytes, CellInput, CellOutput, OutPoint, Script, Transaction, WitnessArgs},
    prelude::*,
};
use molecule::prelude::Entity;
use ractor::{Actor, ActorProcessingErr, ActorRef};
use secp256k1::{Message, SecretKey, SECP256K1};
use strum::AsRefStr;
use tracing::{debug, error, info, warn};

use crate::{
    ckb::{
        config::{new_default_cell_collector, CKB_RPC_TIMEOUT},
        contracts::{get_cell_deps_sync, get_script_by_contract, Contract},
        signer::LocalSigner,
        CkbConfig,
    },
    fiber::channel::{
        settlement_data_to_witness, settlement_tlc_local_pubkey_hash, settlement_tlc_to_witness,
        XUDT_COMPATIBLE_WITNESS,
    },
    fiber::onchain_tlc_reconcile::OnChainTlcSettlement,
    now_timestamp_as_millis_u64,
    utils::{
        actor::ActorHandleLogGuard,
        arithmetic::{
            checked_add_u64, checked_mul_u64, checked_sub_u64, checked_sub_usize, ArithmeticError,
        },
        tx::compute_tx_message,
    },
    watchtower::{
        channel_data_funding_tx_lock, channel_data_local_settlement_pubkey_hash,
        channel_data_x_only_aggregated_pubkey,
    },
};
use fiber_types::{
    ChannelData, Hash256, HashAlgorithm, NodeId, Privkey, Pubkey, RevocationData, SettlementData,
    TLCId,
};

use super::WatchtowerStore;

pub const DEFAULT_WATCHTOWER_CHECK_INTERVAL_SECONDS: u64 = 60;

pub struct WatchtowerActor<S> {
    store: S,
    // a node_id represent the watchtower itself
    node_id: NodeId,
}

const ACTOR_HANDLE_WARN_THRESHOLD_MS: u64 = 15_000;

fn tx_size_with_extra_inputs(
    tx_builder: &TransactionBuilder,
    extra_inputs: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let tx_size = u64::try_from(tx_builder.clone().build().data().serialized_size_in_block())
        .map_err(|_| ArithmeticError::new("transaction size does not fit into u64"))?;
    let extra_size = checked_mul_u64(
        CellInput::TOTAL_SIZE as u64,
        extra_inputs,
        "extra input size",
    )?;
    checked_add_u64(tx_size, extra_size, "transaction size")
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
}

impl<S: WatchtowerStore> WatchtowerActor<S> {
    pub fn new(store: S) -> Self {
        let node_id = NodeId::local();
        Self { store, node_id }
    }
}

#[derive(AsRefStr)]
pub enum WatchtowerMessage {
    CreateChannel(
        Hash256,
        Option<Script>,
        Privkey,
        Pubkey,
        Pubkey,
        Pubkey,
        SettlementData,
    ),
    RemoveChannel(Hash256),
    UpdateRevocation(Hash256, RevocationData, SettlementData),
    UpdatePendingRemoteSettlement(Hash256, SettlementData),
    UpdateLocalSettlement(Hash256, SettlementData),
    CreatePreimage(Hash256, Hash256),
    RemovePreimage(Hash256),
    PeriodicCheck,
}

pub struct WatchtowerState {
    config: CkbConfig,
    signer: LocalSigner,
    /// is periodic check running
    periodic_check_running: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl<S> Actor for WatchtowerActor<S>
where
    S: WatchtowerStore + Send + Sync + Clone + 'static,
{
    type Msg = WatchtowerMessage;
    type State = WatchtowerState;
    type Arguments = CkbConfig;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        config: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let secret_key = config.read_secret_key()?;
        let signer = LocalSigner::new(secret_key);
        Ok(Self::State {
            config,
            signer,
            periodic_check_running: Arc::new(AtomicBool::new(false)),
        })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let _handle_log_guard = ActorHandleLogGuard::new(
            "WatchtowerActor",
            message.as_ref().to_string(),
            "fiber.watchtower_actor",
            ACTOR_HANDLE_WARN_THRESHOLD_MS,
        );
        match message {
            WatchtowerMessage::CreateChannel(
                channel_id,
                funding_udt_type_script,
                local_settlement_key,
                remote_settlement_key,
                local_funding_pubkey,
                remote_funding_pubkey,
                settlement_data,
            ) => self.store.insert_watch_channel(
                NodeId::local(),
                channel_id,
                funding_udt_type_script,
                local_settlement_key,
                remote_settlement_key,
                local_funding_pubkey,
                remote_funding_pubkey,
                settlement_data,
            ),
            WatchtowerMessage::RemoveChannel(channel_id) => {
                self.store.remove_watch_channel(NodeId::local(), channel_id)
            }
            WatchtowerMessage::UpdateRevocation(
                channel_id,
                revocation_data,
                remote_settlement_data,
            ) => self.store.update_revocation(
                NodeId::local(),
                channel_id,
                revocation_data,
                remote_settlement_data,
            ),
            WatchtowerMessage::UpdatePendingRemoteSettlement(
                channel_id,
                pending_remote_settlement_data,
            ) => self.store.update_pending_remote_settlement(
                NodeId::local(),
                channel_id,
                pending_remote_settlement_data,
            ),
            WatchtowerMessage::UpdateLocalSettlement(channel_id, local_settlement_data) => self
                .store
                .update_local_settlement(NodeId::local(), channel_id, local_settlement_data),
            WatchtowerMessage::CreatePreimage(payment_hash, preimage) => {
                if HashAlgorithm::supported_algorithms()
                    .iter()
                    .any(|algorithm| payment_hash == algorithm.hash(preimage).into())
                {
                    self.store
                        .insert_watch_preimage(NodeId::local(), payment_hash, preimage);
                } else {
                    tracing::error!("CreatePreimage with wrong preimage, payment_hash: {payment_hash:?} preimage: {preimage:?}");
                }
            }
            WatchtowerMessage::RemovePreimage(payment_hash) => self
                .store
                .remove_watch_preimage(NodeId::local(), payment_hash),
            WatchtowerMessage::PeriodicCheck => {
                // Check if a periodic check is already running
                if state
                    .periodic_check_running
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    info!("PeriodicCheck is already running, skipping this check");
                    return Ok(());
                }
                // Spawn the periodic check task
                let store = self.store.clone();
                let node_id = self.node_id.clone();
                let rpc_url = state.config.rpc_url.clone();
                let periodic_check_running = state.periodic_check_running.clone();
                let signer = state.signer.clone();
                tokio::task::spawn_blocking(move || {
                    // Use RAII guard to ensure flag is reset even on panic
                    let _guard = PeriodicCheckGuard(periodic_check_running);
                    info!("PeriodicCheck started");
                    let start = now_timestamp_as_millis_u64();
                    run_periodic_check(store, node_id, signer, rpc_url);
                    let elapsed = now_timestamp_as_millis_u64().saturating_sub(start);
                    info!("PeriodicCheck finished elapsed: {}ms", elapsed);
                });
            }
        }
        Ok(())
    }
}

/// RAII guard to ensure `periodic_check_running` is reset even if the task panics
struct PeriodicCheckGuard(Arc<AtomicBool>);

impl Drop for PeriodicCheckGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn run_periodic_check<S>(store: S, _node_id: NodeId, signer: LocalSigner, rpc_url: String)
where
    S: WatchtowerStore + Send + Sync + 'static,
{
    let mut cell_collector = new_default_cell_collector(&rpc_url);

    for (channel_node_id, channel_data) in store.get_watch_channels_with_nodes() {
        let ckb_client = CkbRpcClient::with_builder(&rpc_url, |builder| {
            builder.timeout(CKB_RPC_TIMEOUT).no_proxy()
        })
        .expect("create ckb rpc client should not fail");
        let tx_hash = match crate::ckb::client::find_first_input_tx_hash(
            &ckb_client,
            &channel_data_funding_tx_lock(&channel_data),
        ) {
            Ok(Some(tx_hash)) => tx_hash,
            Ok(None) => continue,
            Err(err) => {
                error!("Failed to get transactions: {:?}", err);
                continue;
            }
        };
        match ckb_client.get_transaction(tx_hash.clone()) {
            Ok(Some(tx_with_status)) => {
                if tx_with_status.tx_status.status != Status::Committed {
                    error!("Cannot find the commitment tx: {:?}, status is {:?}, maybe ckb indexer bug?", tx_with_status.tx_status.status, tx_hash);
                    continue;
                }

                let Some(first_commitment_block_number) = tx_with_status
                    .tx_status
                    .block_number
                    .as_ref()
                    .map(|block_number| block_number.value())
                else {
                    error!(
                        "Cannot find the commitment tx block number: {:?}, maybe ckb indexer bug?",
                        tx_hash
                    );
                    continue;
                };

                if let Some(tx) = tx_with_status.transaction {
                    match tx.inner {
                        Either::Left(tx) => {
                            let tx: Transaction = tx.inner.into();
                            if tx.raw().outputs().len() == 1 {
                                let first_commitment_tx_out_point =
                                    OutPoint::new(tx.calc_tx_hash(), 0);
                                let output = tx.raw().outputs().get(0).expect("get output 0 of tx");
                                let commitment_lock = output.lock();
                                let lock_args = commitment_lock.args().raw_data();
                                let pub_key_hash: [u8; 20] =
                                    lock_args[0..20].try_into().expect("checked length");
                                let commitment_number = u64::from_be_bytes(
                                    lock_args[28..36].try_into().expect("u64 from slice"),
                                );

                                let x_only_aggregated_pubkey =
                                    channel_data_x_only_aggregated_pubkey(&channel_data, false);
                                if blake160(&x_only_aggregated_pubkey).0 == pub_key_hash {
                                    match channel_data.revocation_data.clone() {
                                        Some(revocation_data)
                                            if revocation_data.commitment_number
                                                >= commitment_number =>
                                        {
                                            match ckb_client.get_live_cell(
                                                first_commitment_tx_out_point.clone().into(),
                                                false,
                                            ) {
                                                Ok(cell_with_status) => {
                                                    if cell_with_status.status == "live" {
                                                        warn!("Found an old version commitment tx submitted by remote: {:#x}", tx.calc_tx_hash());
                                                        match build_revocation_tx(
                                                            first_commitment_tx_out_point,
                                                            revocation_data,
                                                            x_only_aggregated_pubkey,
                                                            &signer,
                                                            &mut cell_collector,
                                                        ) {
                                                            Ok(tx) => {
                                                                match ckb_client.send_transaction(
                                                                    tx.data().into(),
                                                                    None,
                                                                ) {
                                                                    Ok(tx_hash) => {
                                                                        info!("Revocation tx: {:?} sent, tx_hash: {:?}", tx, tx_hash);
                                                                    }
                                                                    Err(err) => {
                                                                        error!("Failed to send revocation tx: {:?}, error: {:?}", tx, err);
                                                                    }
                                                                }
                                                            }
                                                            Err(err) => {
                                                                error!("Failed to build revocation tx: {:?}", err);
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(err) => {
                                                    error!("Failed to get live cell: {:?}", err);
                                                }
                                            }
                                        }
                                        _ => {
                                            try_settle_commitment_tx(
                                                commitment_lock,
                                                first_commitment_tx_out_point,
                                                ckb_client,
                                                channel_data,
                                                true,
                                                &signer,
                                                &mut cell_collector,
                                                &store,
                                                channel_node_id.clone(),
                                                first_commitment_block_number,
                                            );
                                        }
                                    }
                                } else {
                                    try_settle_commitment_tx(
                                        commitment_lock,
                                        first_commitment_tx_out_point,
                                        ckb_client,
                                        channel_data,
                                        false,
                                        &signer,
                                        &mut cell_collector,
                                        &store,
                                        channel_node_id.clone(),
                                        first_commitment_block_number,
                                    );
                                }
                            } else {
                                // there may be a race condition that PeriodicCheck is triggered before the remove_channel fn is called
                                // it's a close channel tx, ignore
                            }
                        }
                        Either::Right(_tx) => {
                            // unreachable, ignore
                        }
                    }
                } else {
                    error!("Cannot find the commitment tx: {:?}, transaction is none, maybe ckb indexer bug?", tx_hash);
                }
            }
            Ok(None) => {
                error!(
                    "Cannot find the commitment tx: {:?}, maybe ckb indexer bug?",
                    tx_hash
                );
            }
            Err(err) => {
                error!("Failed to get commitment tx: {:?}", err);
            }
        }
    }
}

fn build_revocation_tx(
    commitment_tx_out_point: OutPoint,
    revocation_data: RevocationData,
    x_only_aggregated_pubkey: [u8; 32],
    signer: &LocalSigner,
    cell_collector: &mut DefaultCellCollector,
) -> Result<TransactionView, Box<dyn std::error::Error>> {
    let witness = [
        XUDT_COMPATIBLE_WITNESS.to_vec(),
        vec![0x00], // unlock_count = 0x00 for revocation
        revocation_data.commitment_number.to_be_bytes().to_vec(),
        x_only_aggregated_pubkey.to_vec(),
        revocation_data.aggregated_signature.serialize().to_vec(),
    ]
    .concat();

    let args = signer.pubkey_hash();
    let fee_provider_lock_script = get_script_by_contract(Contract::Secp256k1Lock, args);

    let change_output = CellOutput::new_builder()
        .lock(fee_provider_lock_script.clone())
        .build();
    let change_output_occupied_capacity = change_output
        .occupied_capacity(Capacity::shannons(0))
        .expect("capacity does not overflow")
        .as_u64();
    let placeholder_witness = WitnessArgs::new_builder()
        .lock(Some(ckb_types::bytes::Bytes::from(vec![0u8; 65])).pack())
        .build();

    let mut tx_builder = Transaction::default()
        .as_advanced_builder()
        .cell_deps(get_cell_deps_sync(
            vec![Contract::CommitmentLock, Contract::Secp256k1Lock],
            &revocation_data.output.type_().to_opt(),
        )?)
        .input(
            CellInput::new_builder()
                .previous_output(commitment_tx_out_point)
                .build(),
        )
        .output(revocation_data.output.clone())
        .output_data(revocation_data.output_data)
        .witness(witness.pack())
        .output(change_output.clone())
        .output_data(Bytes::default())
        .witness(placeholder_witness.as_bytes().pack());

    // TODO: move it to config or use https://github.com/nervosnetwork/ckb/pull/4477
    let fee_calculator = FeeCalculator::new(1000);
    // use two inputs as the maximum fee provider cell inputs
    let fee = fee_calculator.fee(tx_size_with_extra_inputs(&tx_builder, 2)?);
    let min_total_capacity = checked_add_u64(
        change_output_occupied_capacity,
        fee,
        "revocation min capacity",
    )?;
    let mut query = CellQueryOptions::new_lock(fee_provider_lock_script);
    query.script_search_mode = Some(SearchMode::Exact);
    query.secondary_script_len_range = Some(ValueRangeOption::new_exact(0));
    query.data_len_range = Some(ValueRangeOption::new_exact(0));
    query.min_total_capacity = min_total_capacity;
    let (cells, _total_capacity) = cell_collector.collect_live_cells(&query, false)?;
    let mut inputs_capacity = 0u64;
    for cell in cells {
        let input_capacity: u64 = cell.output.capacity().unpack();
        inputs_capacity = checked_add_u64(
            inputs_capacity,
            input_capacity,
            "revocation inputs capacity",
        )?;
        tx_builder = tx_builder.input(
            CellInput::new_builder()
                .previous_output(cell.out_point)
                .build(),
        );
        let tx_size = u64::try_from(tx_builder.clone().build().data().serialized_size_in_block())
            .map_err(|_| {
            ArithmeticError::new("transaction size does not fit into u64".to_string())
        })?;
        let fee = fee_calculator.fee(tx_size);
        let required_capacity = checked_add_u64(
            change_output_occupied_capacity,
            fee,
            "revocation required capacity",
        )?;
        if inputs_capacity >= required_capacity {
            let new_change_output = change_output
                .as_builder()
                .capacity(checked_sub_u64(
                    inputs_capacity,
                    fee,
                    "revocation change capacity",
                )?)
                .build();
            let tx = tx_builder
                .set_outputs(vec![revocation_data.output, new_change_output])
                .build();

            let tx = sign_tx(tx, signer)?;
            return Ok(tx);
        }
    }

    Err(Box::new(RpcError::Other(anyhow!("Not enough capacity"))))
}

#[allow(clippy::too_many_arguments)]
fn try_settle_commitment_tx<S: WatchtowerStore>(
    commitment_lock: Script,
    first_commitment_tx_out_point: OutPoint,
    ckb_client: CkbRpcClient,
    channel_data: ChannelData,
    for_remote: bool,
    signer: &LocalSigner,
    cell_collector: &mut DefaultCellCollector,
    store: &S,
    self_node_id: NodeId,
    first_commitment_block_number: u64,
) {
    let lock_args = commitment_lock.args().raw_data();
    let initial_tlcs = tracked_settlement_tlcs(&commitment_lock, &channel_data, for_remote);
    let script = commitment_lock
        .as_builder()
        .args(lock_args[0..36].to_vec().pack())
        .build();
    let (current_epoch, current_time, tip_block_number) = match ckb_client.get_tip_header() {
        Ok(tip_header) => {
            let tip_block_number = tip_header.inner.number.value();
            match ckb_client.get_block_median_time(tip_header.hash.clone()) {
                Ok(Some(median_time)) => {
                    let tip_header: HeaderView = tip_header.into();
                    let epoch = tip_header.epoch();
                    (epoch, median_time.value(), tip_block_number)
                }
                Ok(None) => {
                    error!(
                        "Cannot find median time: {:?}, ckb rpc bug?",
                        tip_header.hash
                    );
                    return;
                }
                Err(err) => {
                    error!("Failed to get median time: {:?}", err);
                    return;
                }
            }
        }
        Err(err) => {
            error!("Failed to get tip header: {:?}", err);
            return;
        }
    };

    let search_key = SearchKey {
        script: script.clone().into(),
        script_type: ScriptType::Lock,
        script_search_mode: Some(SearchMode::Prefix),
        with_data: None,
        filter: Some(SearchKeyFilter {
            block_range: Some([
                first_commitment_block_number.into(),
                tip_block_number.saturating_add(1).into(),
            ]),
            ..Default::default()
        }),
        group_by_transaction: Some(true),
    };

    let Some(initial_tlcs) = initial_tlcs else {
        error!(
            "Cannot reconstruct settlement TLC identities for channel {:?}; skipping on-chain TLC reconciliation and settlement construction",
            channel_data.channel_id
        );
        return;
    };
    let settlement_scan = scan_watched_settlement_txs(
        search_key.clone(),
        &ckb_client,
        &script,
        first_commitment_tx_out_point.clone(),
        initial_tlcs,
        &channel_data.channel_id,
        store,
        &self_node_id,
    );

    // the live cells number should be 1 or 0 for normal case.
    // however, an attacker may create a lot of cells to implement a tx pinning attack, we have to use loop to get all cells
    let mut after = None;
    loop {
        match ckb_client.get_cells(
            search_key.clone(),
            Order::Desc,
            100u32.into(),
            after.clone(),
        ) {
            Ok(cells) => {
                if cells.objects.is_empty() {
                    break;
                }
                after = Some(cells.last_cursor.clone());
                for cell in cells.objects {
                    let commitment_tx_hash = cell.out_point.tx_hash.clone();
                    let commitment_tx_out_point =
                        OutPoint::new(commitment_tx_hash.pack(), cell.out_point.index.value());
                    let Some(tracked_tlcs) = settlement_scan
                        .tracked_tlcs_by_outpoint
                        .get(&commitment_tx_out_point)
                    else {
                        warn!(
                            "Found a live settlement cell without a verified TLC identity mapping: {:?}",
                            commitment_tx_out_point
                        );
                        continue;
                    };
                    // is it the first commitment tx which has unlocked funding output or not
                    let is_first = commitment_tx_out_point == first_commitment_tx_out_point;
                    let cell_header: HeaderView =
                        match ckb_client.get_header_by_number(cell.block_number) {
                            Ok(Some(header)) => header.into(),
                            Ok(None) => {
                                error!("Cannot find header: {}", cell.block_number);
                                continue;
                            }
                            Err(err) => {
                                error!("Failed to get header: {:?}", err);
                                continue;
                            }
                        };
                    let cell_header_epoch = cell_header.epoch();

                    let settlement_witness = if is_first {
                        None
                    } else {
                        match ckb_client.get_transaction(commitment_tx_hash.clone()) {
                            Ok(Some(tx_with_status)) => {
                                if tx_with_status.tx_status.status != Status::Committed {
                                    error!("Cannot find the commitment tx: {:?}, status is {:?}, maybe ckb indexer bug?", tx_with_status.tx_status.status, commitment_tx_hash);
                                    continue;
                                } else if let Some(tx) = tx_with_status.transaction {
                                    match tx.inner {
                                        Either::Left(tx) => {
                                            let tx: Transaction = tx.inner.into();
                                            let Some(witness_index) = settlement_scan
                                                .witness_input_indices
                                                .get(&commitment_tx_hash.pack())
                                                .copied()
                                            else {
                                                warn!("Found a commitment tx, but it does not spend a watched commitment outpoint: {:?}", commitment_tx_hash);
                                                continue;
                                            };
                                            match tx.witnesses().get(witness_index) {
                                                Some(witness) => {
                                                    let witness = witness.raw_data();
                                                    if witness.len() > 18
                                                        && witness[0..16] == XUDT_COMPATIBLE_WITNESS
                                                    {
                                                        SettlementWitness::build_from_witness(
                                                            &witness[16..],
                                                        )
                                                    } else {
                                                        warn!("Found a commitment tx, but the witness is invalid: {:?}", commitment_tx_hash);
                                                        continue;
                                                    }
                                                }
                                                None => {
                                                    warn!("Found a commitment tx, but the witnesses are empty: {:?}", commitment_tx_hash);
                                                    continue;
                                                }
                                            }
                                        }
                                        Either::Right(_) => {
                                            // unreachable, ignore
                                            continue;
                                        }
                                    }
                                } else {
                                    error!("Cannot find the commitment tx: {:?}, transaction is none, maybe ckb indexer bug?", commitment_tx_hash);
                                    continue;
                                }
                            }
                            Ok(None) => {
                                error!(
                                    "Cannot find the commitment tx: {:?}, maybe ckb indexer bug?",
                                    commitment_tx_hash
                                );
                                continue;
                            }
                            Err(err) => {
                                error!("Failed to get commitment tx: {:?}", err);
                                continue;
                            }
                        }
                    };
                    match build_settlement_tx(
                        cell,
                        cell_header_epoch,
                        current_epoch,
                        current_time,
                        &self_node_id,
                        for_remote,
                        channel_data.clone(),
                        settlement_witness,
                        tracked_tlcs,
                        signer,
                        cell_collector,
                        store,
                    ) {
                        Ok(Some(tx)) => match ckb_client.send_transaction(tx.data().into(), None) {
                            Ok(tx_hash) => {
                                info!("Settlement tx: {:?} sent, tx_hash: {:#x}", tx, tx_hash);
                            }
                            Err(err) => {
                                error!("Failed to send settlement tx: {:?}, error: {:?}", tx, err);
                            }
                        },
                        Ok(None) => {
                            // ignore, the tx is not ready to settle
                        }
                        Err(err) => {
                            error!("Failed to build settlement tx: {:?}", err);
                        }
                    }
                }
            }
            Err(err) => {
                error!("Failed to get cells: {:?}, aborting settlement scan", err);
                break;
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackedSettlementTlc {
    tlc_id: TLCId,
    payment_hash: Hash256,
    hash_algorithm: HashAlgorithm,
    witness: Vec<u8>,
}

struct WatchedSettlementScan {
    witness_input_indices: HashMap<ckb_types::packed::Byte32, usize>,
    tracked_tlcs_by_outpoint: HashMap<OutPoint, Vec<TrackedSettlementTlc>>,
}

fn settlement_data_for_commitment(
    channel_data: &ChannelData,
    for_remote: bool,
    commitment_number: u64,
) -> &SettlementData {
    if for_remote {
        if channel_data
            .revocation_data
            .as_ref()
            .and_then(|revocation| {
                commitment_number
                    .checked_sub(1)
                    .map(|previous| revocation.commitment_number == previous)
            })
            .unwrap_or(false)
        {
            &channel_data.remote_settlement_data
        } else {
            &channel_data.pending_remote_settlement_data
        }
    } else {
        &channel_data.local_settlement_data
    }
}

fn tracked_settlement_tlcs(
    commitment_lock: &Script,
    channel_data: &ChannelData,
    for_remote: bool,
) -> Option<Vec<TrackedSettlementTlc>> {
    let lock_args = commitment_lock.args().raw_data();
    if lock_args.len() < 56 {
        return None;
    }
    let commitment_number = u64::from_be_bytes(lock_args[28..36].try_into().ok()?);
    let settlement_data =
        settlement_data_for_commitment(channel_data, for_remote, commitment_number);
    let committed_witness_hash = &lock_args[36..56];
    let settlement_witness = settlement_data_to_witness(
        settlement_data,
        for_remote,
        channel_data.local_settlement_key.clone(),
        channel_data.remote_settlement_key,
    );
    if blake160(&settlement_witness).as_ref() != committed_witness_hash {
        warn!(
            "Settlement snapshot hash does not match commitment lock for channel {:?}, commitment {}",
            channel_data.channel_id, commitment_number
        );
        return None;
    }

    Some(
        settlement_data
            .tlcs
            .iter()
            .map(|tlc| TrackedSettlementTlc {
                tlc_id: if for_remote {
                    tlc.tlc_id
                } else {
                    tlc.tlc_id.flip()
                },
                payment_hash: tlc.payment_hash,
                hash_algorithm: tlc.hash_algorithm,
                witness: settlement_tlc_to_witness(tlc, for_remote),
            })
            .collect(),
    )
}

#[allow(clippy::too_many_arguments)]
fn scan_watched_settlement_txs<S: WatchtowerStore>(
    search_key: SearchKey,
    ckb_client: &CkbRpcClient,
    commitment_lock_prefix: &Script,
    first_commitment_tx_out_point: OutPoint,
    initial_tlcs: Vec<TrackedSettlementTlc>,
    channel_id: &Hash256,
    store: &S,
    self_node_id: &NodeId,
) -> WatchedSettlementScan {
    let mut watched_outpoints = HashMap::from([(first_commitment_tx_out_point, initial_tlcs)]);
    let mut settlement_witness_input_indices = HashMap::new();

    let mut candidates: Vec<Transaction> = Vec::new();
    let mut after = None;
    loop {
        match ckb_client.get_transactions(
            search_key.clone(),
            Order::Asc,
            100u32.into(),
            after.clone(),
        ) {
            Ok(txs) => {
                if txs.objects.is_empty() {
                    break;
                }
                after = Some(txs.last_cursor.clone());
                for indexed_tx in txs.objects {
                    match ckb_client.get_transaction(indexed_tx.tx_hash()) {
                        Ok(Some(tx_with_status)) => {
                            if tx_with_status.tx_status.status != Status::Committed {
                                error!("Cannot find the tx: {:?}, status is {:?}, maybe ckb indexer bug?", tx_with_status.tx_status.status, indexed_tx.tx_hash());
                            } else if let Some(tx) = tx_with_status.transaction {
                                match tx.inner {
                                    Either::Left(tx) => {
                                        let tx: Transaction = tx.inner.into();
                                        candidates.push(tx);
                                    }
                                    Either::Right(_) => {
                                        // unreachable, ignore
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            error!(
                                "Cannot find the tx: {:?}, maybe ckb indexer bug?",
                                indexed_tx.tx_hash()
                            );
                        }
                        Err(err) => {
                            error!("Failed to get tx: {:?}", err);
                        }
                    }
                }
            }
            Err(err) => {
                error!(
                    "Failed to get transactions: {:?}, aborting settlement scan",
                    err
                );
                break;
            }
        }
    }

    let mut processed_tx_hashes = HashSet::new();
    loop {
        let mut progress = false;
        for tx in &candidates {
            let tx_hash = tx.calc_tx_hash();
            if processed_tx_hashes.contains(&tx_hash) {
                continue;
            }
            if let Some(witness_index) = process_watched_settlement_tx(
                tx,
                &mut watched_outpoints,
                &mut processed_tx_hashes,
                commitment_lock_prefix,
                channel_id,
                store,
                self_node_id,
            ) {
                settlement_witness_input_indices.insert(tx_hash, witness_index);
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    WatchedSettlementScan {
        witness_input_indices: settlement_witness_input_indices,
        tracked_tlcs_by_outpoint: watched_outpoints,
    }
}

fn lock_matches_commitment_prefix(lock: &Script, commitment_lock_prefix: &Script) -> bool {
    let lock_args = lock.args().raw_data();
    lock.code_hash() == commitment_lock_prefix.code_hash()
        && lock.hash_type() == commitment_lock_prefix.hash_type()
        && lock_args.starts_with(commitment_lock_prefix.args().raw_data().as_ref())
}

fn process_watched_settlement_tx<S: WatchtowerStore>(
    tx: &Transaction,
    watched_outpoints: &mut HashMap<OutPoint, Vec<TrackedSettlementTlc>>,
    processed_tx_hashes: &mut HashSet<ckb_types::packed::Byte32>,
    commitment_lock_prefix: &Script,
    channel_id: &Hash256,
    store: &S,
    self_node_id: &NodeId,
) -> Option<usize> {
    let tx_hash = tx.calc_tx_hash();
    if processed_tx_hashes.contains(&tx_hash) {
        return None;
    }
    // The commitment-lock contract reads `load_witness(0, Source::GroupInput)`.
    // Since it enforces a single group input, that witness is stored at the
    // global input index that spends the watched commitment/settlement cell,
    // not necessarily at `witnesses[0]`.
    let inputs = tx.raw().inputs();
    let (watched_input_index, watched_outpoint) = (0..inputs.len()).find_map(|index| {
        let previous_output = inputs
            .get(index)
            .expect("input index checked")
            .previous_output();
        watched_outpoints
            .contains_key(&previous_output)
            .then_some((index, previous_output))
    })?;

    let tracked_tlcs = watched_outpoints.get(&watched_outpoint)?.clone();
    let remaining_tlcs = reconcile_settlement_witness(
        tx,
        watched_input_index,
        channel_id,
        store,
        self_node_id,
        &tracked_tlcs,
    )?;
    processed_tx_hashes.insert(tx_hash.clone());
    watched_outpoints.remove(&watched_outpoint);

    let outputs = tx.raw().outputs();
    if let Some(output) = outputs.get(0) {
        if lock_matches_commitment_prefix(&output.lock(), commitment_lock_prefix) {
            watched_outpoints.insert(OutPoint::new(tx_hash, 0), remaining_tlcs);
        }
    }

    Some(watched_input_index)
}

/// Validate a settlement witness against the tracked snapshot, persist exact TLC proofs, and
/// return the still-pending TLCs in their next-witness order.
fn reconcile_settlement_witness<S: WatchtowerStore>(
    tx: &Transaction,
    witness_index: usize,
    channel_id: &Hash256,
    store: &S,
    self_node_id: &NodeId,
    tracked_tlcs: &[TrackedSettlementTlc],
) -> Option<Vec<TrackedSettlementTlc>> {
    let Some(witness) = tx.witnesses().get(witness_index) else {
        warn!(
            "Found a commitment tx, but the witnesses are empty: {:?}",
            tx.calc_tx_hash()
        );
        return None;
    };
    let witness = witness.raw_data();
    if witness.len() <= 18 || witness[0..16] != XUDT_COMPATIBLE_WITNESS {
        warn!(
            "Found a commitment tx with an invalid settlement witness: {:?}",
            tx.calc_tx_hash()
        );
        return None;
    }
    let Some(settlement_witness) = SettlementWitness::build_from_witness(&witness[16..]) else {
        warn!(
            "Cannot decode settlement witness for tx {:?}",
            tx.calc_tx_hash()
        );
        return None;
    };
    if settlement_witness.pending_htlcs.len() != tracked_tlcs.len()
        || settlement_witness
            .pending_htlcs
            .iter()
            .zip(tracked_tlcs)
            .any(|(witness_tlc, tracked_tlc)| witness_tlc.to_witness() != tracked_tlc.witness)
    {
        warn!(
            "Settlement witness TLC list does not match the tracked commitment snapshot for channel {:?}, tx {:?}",
            channel_id,
            tx.calc_tx_hash()
        );
        return None;
    }

    let tx_hash: Hash256 = tx.calc_tx_hash().into();
    let mut settled_indices = HashSet::new();
    let mut final_party_unlock = false;
    for unlock in settlement_witness.unlocks {
        if unlock.unlock_type >= 0xFE {
            final_party_unlock = true;
            debug!(
                "watchtower observed final party unlock 0x{:02x} for channel {:?} with {} pending TLCs tx {:?}",
                unlock.unlock_type,
                channel_id,
                tracked_tlcs.len(),
                tx.calc_tx_hash(),
            );
            continue;
        }

        let index = unlock.unlock_type as usize;
        let Some(tlc) = tracked_tlcs.get(index) else {
            warn!(
                "Settlement witness references invalid TLC index {} for channel {:?}, tx {:?}",
                index,
                channel_id,
                tx.calc_tx_hash()
            );
            return None;
        };
        settled_indices.insert(index);
        let preimage = if unlock.with_preimage {
            Some(unlock.preimage?)
        } else {
            None
        };
        if let Some(preimage) = preimage {
            let discovered_payment_hash: Hash256 = tlc.hash_algorithm.hash(preimage).into();
            if discovered_payment_hash == tlc.payment_hash {
                store.insert_watch_preimage(self_node_id.clone(), tlc.payment_hash, preimage);
            } else {
                warn!(
                    "On-chain preimage for channel {:?} tlc {:?} tx {:?} hashes to {:?}, expected full hash {:?}",
                    channel_id,
                    tlc.tlc_id,
                    tx.calc_tx_hash(),
                    discovered_payment_hash,
                    tlc.payment_hash
                );
            }
        }
        store.insert_onchain_tlc_settlement(
            channel_id,
            tlc.tlc_id,
            OnChainTlcSettlement {
                payment_hash: tlc.payment_hash,
                hash_algorithm: tlc.hash_algorithm,
                preimage,
                tx_hash,
                tlc_index: unlock.unlock_type,
            },
        );
    }

    if final_party_unlock {
        for (index, tlc) in tracked_tlcs.iter().enumerate() {
            if settled_indices.contains(&index) {
                continue;
            }
            store.insert_onchain_tlc_settlement(
                channel_id,
                tlc.tlc_id,
                OnChainTlcSettlement {
                    payment_hash: tlc.payment_hash,
                    hash_algorithm: tlc.hash_algorithm,
                    preimage: None,
                    tx_hash,
                    tlc_index: index as u8,
                },
            );
        }
        return Some(Vec::new());
    }

    Some(
        tracked_tlcs
            .iter()
            .enumerate()
            .filter(|(index, _)| !settled_indices.contains(index))
            .map(|(_, tlc)| tlc.clone())
            .collect(),
    )
}

fn verified_watch_preimage<S: WatchtowerStore>(
    store: &S,
    self_node_id: &NodeId,
    tracked_tlc: &TrackedSettlementTlc,
) -> Option<Hash256> {
    let preimage = store.get_watch_preimage(self_node_id, &tracked_tlc.payment_hash)?;
    let discovered_payment_hash: Hash256 = tracked_tlc.hash_algorithm.hash(preimage).into();
    if discovered_payment_hash == tracked_tlc.payment_hash {
        Some(preimage)
    } else {
        warn!(
            "Ignoring watchtower preimage for tlc {:?}: derived hash {:?} using {:?}, expected {:?}",
            tracked_tlc.tlc_id,
            discovered_payment_hash,
            tracked_tlc.hash_algorithm,
            tracked_tlc.payment_hash,
        );
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn build_settlement_tx<S: WatchtowerStore>(
    commitment_cell: Cell,
    cell_header_epoch: EpochNumberWithFraction,
    current_epoch: EpochNumberWithFraction,
    current_time: u64,
    self_node_id: &NodeId,
    for_remote: bool,
    channel_data: ChannelData,
    settlement_witness: Option<SettlementWitness>,
    tracked_tlcs: &[TrackedSettlementTlc],
    signer: &LocalSigner,
    cell_collector: &mut DefaultCellCollector,
    store: &S,
) -> Result<Option<TransactionView>, Box<dyn std::error::Error>> {
    let cell_output: CellOutput = commitment_cell.output.clone().into();
    let lock_script_args = cell_output.lock().args().raw_data();
    let since = u64::from_le_bytes(lock_script_args[20..28].try_into().expect("u64 from slice"));
    let commitment_number =
        u64::from_be_bytes(lock_script_args[28..36].try_into().expect("u64 from slice"));

    let delay_epoch = {
        let since = Since::from_raw_value(since);
        since
            .is_relative()
            .then(|| {
                since.extract_metric().and_then(|(since_type, value)| {
                    if since_type == SinceType::EpochNumberWithFraction {
                        Some(EpochNumberWithFraction::from_full_value(value))
                    } else {
                        None
                    }
                })
            })
            .flatten()
    };

    if delay_epoch.is_none() {
        return Err(Box::new(RpcError::Other(anyhow!(
            "Found an invalid since commitment cell {:?}",
            commitment_cell
        ))));
    }
    let mut delay_epoch = delay_epoch.unwrap();
    let is_first_settlement = settlement_witness.is_none();
    let settlement_data =
        settlement_data_for_commitment(&channel_data, for_remote, commitment_number).clone();

    let fee_provider_lock_script =
        get_script_by_contract(Contract::Secp256k1Lock, signer.pubkey_hash());
    let change_output = CellOutput::new_builder()
        .lock(fee_provider_lock_script.clone())
        .build();

    let mut two_parties_all_settled = false;
    let (unlock, mut unlock_amount, unlock_key, new_settlement_witness) = match settlement_witness {
        Some(mut sw) => {
            if sw.update() {
                if sw.pending_htlcs.len() != tracked_tlcs.len()
                    || sw.pending_htlcs.iter().zip(tracked_tlcs).any(
                        |(witness_tlc, tracked_tlc)| {
                            witness_tlc.to_witness() != tracked_tlc.witness
                        },
                    )
                {
                    warn!(
                        "Current settlement witness does not match the verified TLC identity mapping for channel {:?}",
                        channel_data.channel_id
                    );
                    return Ok(None);
                }
                debug!("channel_data local_settlement_key pubkey hash: {:?}，sw settlement_remote_pubkey_hash: {:?}, sw settlement_local_pubkey_hash: {:?}, for_remote: {}",
                    channel_data_local_settlement_pubkey_hash(&channel_data), sw.settlement_remote_pubkey_hash, sw.settlement_local_pubkey_hash, for_remote);
                if for_remote {
                    if sw.settlement_local_pubkey_hash
                        == channel_data_local_settlement_pubkey_hash(&channel_data)
                    {
                        two_parties_all_settled = sw.settlement_remote_pubkey_hash == [0u8; 20];
                        if two_parties_all_settled {
                            (
                                Unlock {
                                    unlock_type: 0xFF,
                                    with_preimage: false,
                                    signature: [0u8; 65],
                                    preimage: None,
                                },
                                sw.settlement_local_amount,
                                channel_data.local_settlement_key.clone(),
                                sw.to_witness(),
                            )
                        } else {
                            let mut pending_tlcs_count = sw.pending_htlcs.len();
                            let mut unlock_option = None;
                            for (i, tlc) in sw.pending_htlcs.iter().enumerate() {
                                let expiry = match tlc.absolute_expiry() {
                                    Some(expiry) => expiry,
                                    None => continue,
                                };

                                if tlc.is_offered() {
                                    if let Some(private_key) =
                                        tlc.find_matched_private_key(&settlement_data, false)
                                    {
                                        if current_time > expiry {
                                            unlock_option = Some((
                                                Unlock {
                                                    unlock_type: i as u8,
                                                    with_preimage: false,
                                                    signature: [0u8; 65],
                                                    preimage: None,
                                                },
                                                tlc.payment_amount,
                                                private_key.clone(),
                                            ));
                                            break;
                                        }
                                    } else {
                                        warn!("Can not find private key for tlc: {:?}, settlement tlcs: {:?}", tlc, settlement_data.tlcs.iter().collect::<Vec<_>>());
                                    }
                                } else if let Some(private_key) =
                                    tlc.find_matched_private_key(&settlement_data, true)
                                {
                                    if let Some(preimage) = verified_watch_preimage(
                                        store,
                                        self_node_id,
                                        &tracked_tlcs[i],
                                    ) {
                                        unlock_option = Some((
                                            Unlock {
                                                unlock_type: i as u8,
                                                with_preimage: true,
                                                signature: [0u8; 65],
                                                preimage: Some(preimage),
                                            },
                                            tlc.payment_amount,
                                            private_key.clone(),
                                        ));
                                        break;
                                    } else if current_time > expiry {
                                        pending_tlcs_count = checked_sub_usize(
                                            pending_tlcs_count,
                                            1,
                                            "pending TLC count",
                                        )?;
                                    }
                                } else {
                                    warn!("Can not find private key for tlc: {:?}, settlement tlcs: {:?}", tlc, settlement_data.tlcs.iter().collect::<Vec<_>>());
                                }
                            }

                            if pending_tlcs_count == 0 {
                                unlock_option = Some((
                                    Unlock {
                                        unlock_type: 0xFF,
                                        with_preimage: false,
                                        signature: [0u8; 65],
                                        preimage: None,
                                    },
                                    sw.settlement_local_amount,
                                    channel_data.local_settlement_key.clone(),
                                ));
                            }

                            if let Some((unlock, unlock_amount, private_key)) = unlock_option {
                                debug!("unlock: {:?}, unlock_amount: {:?}", unlock, unlock_amount);
                                (unlock, unlock_amount, private_key, sw.to_witness())
                            } else {
                                return Ok(None);
                            }
                        }
                    } else {
                        return Ok(None);
                    }
                } else if sw.settlement_remote_pubkey_hash
                    == channel_data_local_settlement_pubkey_hash(&channel_data)
                {
                    two_parties_all_settled = sw.settlement_local_pubkey_hash == [0u8; 20];
                    if two_parties_all_settled {
                        (
                            Unlock {
                                unlock_type: 0xFE,
                                with_preimage: false,
                                signature: [0u8; 65],
                                preimage: None,
                            },
                            sw.settlement_remote_amount,
                            channel_data.local_settlement_key.clone(),
                            sw.to_witness(),
                        )
                    } else {
                        let mut pending_tlcs_count = sw.pending_htlcs.len();
                        let mut unlock_option = None;
                        for (i, tlc) in sw.pending_htlcs.iter().enumerate() {
                            let expiry = match tlc.absolute_expiry() {
                                Some(expiry) => expiry,
                                None => continue,
                            };

                            if !tlc.is_offered() {
                                if let Some(private_key) =
                                    tlc.find_matched_private_key(&settlement_data, false)
                                {
                                    if current_time > expiry {
                                        unlock_option = Some((
                                            Unlock {
                                                unlock_type: i as u8,
                                                with_preimage: false,
                                                signature: [0u8; 65],
                                                preimage: None,
                                            },
                                            tlc.payment_amount,
                                            private_key.clone(),
                                        ));
                                        break;
                                    }
                                } else {
                                    warn!("Can not find private key for tlc: {:?}, settlement tlcs: {:?}", tlc, settlement_data.tlcs.iter().collect::<Vec<_>>());
                                }
                            } else if let Some(private_key) =
                                tlc.find_matched_private_key(&settlement_data, true)
                            {
                                if let Some(preimage) =
                                    verified_watch_preimage(store, self_node_id, &tracked_tlcs[i])
                                {
                                    unlock_option = Some((
                                        Unlock {
                                            unlock_type: i as u8,
                                            with_preimage: true,
                                            signature: [0u8; 65],
                                            preimage: Some(preimage),
                                        },
                                        tlc.payment_amount,
                                        private_key.clone(),
                                    ));
                                    break;
                                } else if current_time > expiry {
                                    pending_tlcs_count = checked_sub_usize(
                                        pending_tlcs_count,
                                        1,
                                        "pending TLC count",
                                    )?;
                                }
                            } else {
                                warn!(
                                    "Can not find private key for tlc: {:?}, settlement tlcs: {:?}",
                                    tlc,
                                    settlement_data.tlcs.iter().collect::<Vec<_>>()
                                );
                            }
                        }

                        if pending_tlcs_count == 0 {
                            unlock_option = Some((
                                Unlock {
                                    unlock_type: 0xFE,
                                    with_preimage: false,
                                    signature: [0u8; 65],
                                    preimage: None,
                                },
                                sw.settlement_remote_amount,
                                channel_data.local_settlement_key.clone(),
                            ));
                        }

                        if let Some((unlock, unlock_amount, private_key)) = unlock_option {
                            debug!("unlock: {:?}, unlock_amount: {:?}", unlock, unlock_amount);
                            (unlock, unlock_amount, private_key, sw.to_witness())
                        } else {
                            return Ok(None);
                        }
                    }
                } else {
                    return Ok(None);
                }
            } else {
                return Err(Box::new(RpcError::Other(anyhow!(
                    "Found an invalid witness commitment cell {:?}",
                    commitment_cell
                ))));
            }
        }
        None => {
            if settlement_data.tlcs.len() != tracked_tlcs.len()
                || settlement_data.tlcs.iter().zip(tracked_tlcs).any(
                    |(settlement_tlc, tracked_tlc)| {
                        settlement_tlc_to_witness(settlement_tlc, for_remote) != tracked_tlc.witness
                    },
                )
            {
                warn!(
                    "Initial settlement data does not match the verified TLC identity mapping for channel {:?}",
                    channel_data.channel_id
                );
                return Ok(None);
            }
            let mut pending_tlcs_count = settlement_data.tlcs.len();
            let mut unlock_option = None;
            for (i, tlc) in settlement_data.tlcs.iter().enumerate() {
                match (tlc.tlc_id.is_offered(), for_remote) {
                    (true, true) | (false, false) => {
                        let delay = mul(delay_epoch, 2, 3).ok_or_else(|| {
                            ArithmeticError::new("delay epoch calculation overflows".to_string())
                        })?;
                        if cell_header_epoch.to_rational() + delay.to_rational()
                            <= current_epoch.to_rational()
                            && current_time > tlc.expiry
                        {
                            unlock_option = Some((
                                Unlock {
                                    unlock_type: i as u8,
                                    with_preimage: false,
                                    signature: [0u8; 65],
                                    preimage: None,
                                },
                                tlc.payment_amount,
                                tlc.local_key.clone(),
                            ));
                            delay_epoch = delay;
                            break;
                        }
                    }
                    _ => {
                        let delay = mul(delay_epoch, 1, 3).ok_or_else(|| {
                            ArithmeticError::new("delay epoch calculation overflows".to_string())
                        })?;
                        if cell_header_epoch.to_rational() + delay.to_rational()
                            <= current_epoch.to_rational()
                        {
                            if let Some(preimage) =
                                verified_watch_preimage(store, self_node_id, &tracked_tlcs[i])
                            {
                                unlock_option = Some((
                                    Unlock {
                                        unlock_type: i as u8,
                                        with_preimage: true,
                                        signature: [0u8; 65],
                                        preimage: Some(preimage),
                                    },
                                    tlc.payment_amount,
                                    tlc.local_key.clone(),
                                ));
                                delay_epoch = delay;
                                break;
                            } else if cell_header_epoch.to_rational() + delay_epoch.to_rational()
                                <= current_epoch.to_rational()
                                && current_time > tlc.expiry
                            {
                                pending_tlcs_count =
                                    checked_sub_usize(pending_tlcs_count, 1, "pending TLC count")?;
                            }
                        }
                    }
                }
            }

            if pending_tlcs_count == 0 {
                if cell_header_epoch.to_rational() + delay_epoch.to_rational()
                    > current_epoch.to_rational()
                {
                    debug!(
                        "Commitment cell: {:?} is not ready to settle local",
                        commitment_cell.out_point.tx_hash
                    );
                    return Ok(None);
                }
                unlock_option = Some((
                    Unlock {
                        unlock_type: if for_remote { 0xFF } else { 0xFE },
                        with_preimage: false,
                        signature: [0u8; 65],
                        preimage: None,
                    },
                    settlement_data.local_amount,
                    channel_data.local_settlement_key.clone(),
                ));
            }

            if let Some((unlock, unlock_amount, private_key)) = unlock_option {
                debug!("unlock: {:?}, unlock_amount: {:?}", unlock, unlock_amount);
                (
                    unlock,
                    unlock_amount,
                    private_key,
                    settlement_data_to_witness(
                        &settlement_data,
                        for_remote,
                        channel_data.local_settlement_key.clone(),
                        channel_data.remote_settlement_key,
                    ),
                )
            } else {
                return Ok(None);
            }
        }
    };

    let mut new_commitment_lock_script_args = lock_script_args[0..36].to_vec();
    let new_script_hash = {
        let mut sw = SettlementWitness::build_from_witness(
            &[&[0x01], new_settlement_witness.as_slice()].concat(),
        )
        .expect("valid data");
        sw.unlocks.push(unlock.clone());
        sw.update();
        blake160(&sw.to_witness()).0
    };
    new_commitment_lock_script_args.extend_from_slice(&new_script_hash);
    new_commitment_lock_script_args.extend_from_slice(&[0x01]);

    let placeholder_witness_for_change = WitnessArgs::new_builder()
        .lock(Some(ckb_types::bytes::Bytes::from(vec![0u8; 65])).pack())
        .build();

    if cell_output.type_().is_none() {
        let capacity: u64 = cell_output.capacity().unpack();
        if two_parties_all_settled {
            unlock_amount = capacity as u128;
        }
        let new_capacity = (capacity as u128).saturating_sub(unlock_amount) as u64;
        let new_commitment_output = cell_output
            .clone()
            .as_builder()
            .lock(
                cell_output
                    .lock()
                    .as_builder()
                    .args(new_commitment_lock_script_args.pack())
                    .build(),
            )
            .capacity(new_capacity)
            .build();
        let settlement_output = CellOutput::new_builder()
            .lock(fee_provider_lock_script.clone())
            .capacity(unlock_amount as u64)
            .build();

        let witness_for_commitment_cell: Vec<u8> = [
            XUDT_COMPATIBLE_WITNESS.as_slice(),
            &[0x01],
            new_settlement_witness.as_slice(),
            unlock.to_witness().as_slice(),
        ]
        .concat();

        let input = if is_first_settlement {
            let since = Since::new(
                SinceType::EpochNumberWithFraction,
                delay_epoch.full_value(),
                true,
            )
            .value();
            CellInput::new_builder()
                .previous_output(commitment_cell.out_point.clone())
                .since(since)
                .build()
        } else {
            CellInput::new_builder()
                .previous_output(commitment_cell.out_point.clone())
                .build()
        };

        let mut tx_builder = Transaction::default()
            .as_advanced_builder()
            .cell_deps(get_cell_deps_sync(
                vec![Contract::CommitmentLock, Contract::Secp256k1Lock],
                &None,
            )?)
            .input(input);
        if !two_parties_all_settled {
            tx_builder = tx_builder
                .output(new_commitment_output.clone())
                .output_data(Bytes::default());
        }
        tx_builder = tx_builder
            .output(settlement_output.clone())
            .output_data(Bytes::default())
            .witness(witness_for_commitment_cell.pack())
            .witness(placeholder_witness_for_change.as_bytes().pack());

        // TODO: move it to config or use https://github.com/nervosnetwork/ckb/pull/4477
        let fee_calculator = FeeCalculator::new(1000);
        // use two inputs as the maximum fee provider cell inputs
        let fee = fee_calculator.fee(tx_size_with_extra_inputs(&tx_builder, 2)?);
        let settlement_output_occupied_capacity = settlement_output
            .occupied_capacity(Capacity::shannons(0))
            .map_err(|err| {
                ArithmeticError::new(format!(
                    "settlement output occupied capacity calculation failed: {}",
                    err
                ))
            })?
            .as_u64();
        let required_capacity = checked_add_u64(
            new_capacity,
            settlement_output_occupied_capacity,
            "settlement required capacity",
        )
        .and_then(|amount| checked_add_u64(amount, fee, "settlement required capacity"))?;
        let min_total_capacity = if capacity > required_capacity {
            checked_sub_u64(
                capacity,
                required_capacity,
                "settlement fee provider capacity",
            )?
        } else {
            0
        };
        let mut query = CellQueryOptions::new_lock(fee_provider_lock_script);
        query.script_search_mode = Some(SearchMode::Exact);
        query.secondary_script_len_range = Some(ValueRangeOption::new_exact(0));
        query.data_len_range = Some(ValueRangeOption::new_exact(0));
        if min_total_capacity > 0 {
            query.min_total_capacity = min_total_capacity;
        }
        let (cells, _total_capacity) = cell_collector.collect_live_cells(&query, false)?;
        let mut inputs_capacity = capacity;
        let since = if unlock.unlock_type < 0xFE && !unlock.with_preimage {
            Since::new(SinceType::Timestamp, current_time / 1000, false).value()
        } else {
            0
        };
        for cell in cells {
            let input_capacity: u64 = cell.output.capacity().unpack();
            inputs_capacity = checked_add_u64(
                inputs_capacity,
                input_capacity,
                "settlement inputs capacity",
            )?;
            tx_builder = tx_builder.input(
                CellInput::new_builder()
                    .previous_output(cell.out_point)
                    .since(since)
                    .build(),
            );
            let tx_size =
                u64::try_from(tx_builder.clone().build().data().serialized_size_in_block())
                    .map_err(|_| {
                        ArithmeticError::new("transaction size does not fit into u64".to_string())
                    })?;
            let fee = fee_calculator.fee(tx_size);
            let required_capacity = checked_add_u64(
                new_capacity,
                settlement_output_occupied_capacity,
                "settlement required capacity",
            )
            .and_then(|amount| checked_add_u64(amount, fee, "settlement required capacity"))?;
            if inputs_capacity >= required_capacity {
                let adjusted_settlement_output = change_output
                    .as_builder()
                    .capacity(
                        checked_sub_u64(
                            inputs_capacity,
                            new_capacity,
                            "settlement output capacity",
                        )
                        .and_then(|amount| {
                            checked_sub_u64(amount, fee, "settlement output capacity")
                        })?,
                    )
                    .build();
                let outputs = if two_parties_all_settled {
                    vec![adjusted_settlement_output]
                } else {
                    vec![new_commitment_output, adjusted_settlement_output]
                };
                let tx = tx_builder.set_outputs(outputs).build();
                let tx = sign_tx_with_settlement(tx, signer, unlock_key.0, unlock.with_preimage)?;
                return Ok(Some(tx));
            }
        }

        Err(Box::new(RpcError::Other(anyhow!("Not enough capacity"))))
    } else {
        let output_data = commitment_cell.output_data.as_ref().unwrap();
        if output_data.len() < 16 {
            return Err(Box::new(RpcError::Other(anyhow!("Invalid output data"))));
        }
        let amount = u128::from_le_bytes(output_data.as_bytes()[0..16].try_into().unwrap());
        let new_amount = amount.saturating_sub(unlock_amount);
        let mut new_commitment_output = cell_output
            .clone()
            .as_builder()
            .lock(
                cell_output
                    .lock()
                    .as_builder()
                    .args(new_commitment_lock_script_args.pack())
                    .build(),
            )
            .build();
        let new_commitment_output_data = new_amount.to_le_bytes().to_vec().pack();

        let settlement_output = CellOutput::new_builder()
            .lock(fee_provider_lock_script.clone())
            .type_(cell_output.type_().clone())
            .build();
        let settlement_output_data = if two_parties_all_settled {
            amount
        } else {
            unlock_amount
        }
        .to_le_bytes()
        .to_vec()
        .pack();
        let settlement_output_occupied_capacity = settlement_output
            .occupied_capacity(Capacity::bytes(settlement_output_data.raw_data().len()).unwrap())
            .expect("capacity does not overflow")
            .as_u64();
        let mut settlement_output = settlement_output
            .as_builder()
            .capacity(settlement_output_occupied_capacity)
            .build();

        let witness_for_commitment_cell: Vec<u8> = [
            XUDT_COMPATIBLE_WITNESS.as_slice(),
            &[0x01],
            new_settlement_witness.as_slice(),
            unlock.to_witness().as_slice(),
        ]
        .concat();

        let input = if is_first_settlement {
            let since = Since::new(
                SinceType::EpochNumberWithFraction,
                delay_epoch.full_value(),
                true,
            )
            .value();
            CellInput::new_builder()
                .previous_output(commitment_cell.out_point.clone())
                .since(since)
                .build()
        } else {
            CellInput::new_builder()
                .previous_output(commitment_cell.out_point.clone())
                .build()
        };

        let mut tx_builder = Transaction::default()
            .as_advanced_builder()
            .cell_deps(get_cell_deps_sync(
                vec![Contract::CommitmentLock, Contract::Secp256k1Lock],
                &commitment_cell.output.type_.map(|script| script.into()),
            )?)
            .input(input);

        let outputs_capacity: u64 = if unlock.unlock_type >= 0xFE {
            if two_parties_all_settled {
                settlement_output = settlement_output
                    .as_builder()
                    .capacity(new_commitment_output.capacity())
                    .build();
                settlement_output.capacity().unpack()
            } else {
                let new_commitment_output_capacity = new_commitment_output
                    .occupied_capacity(Capacity::bytes(16).unwrap())
                    .map_err(|err| {
                        ArithmeticError::new(format!(
                            "commitment output occupied capacity calculation failed: {}",
                            err
                        ))
                    })?
                    .as_u64();
                new_commitment_output = new_commitment_output
                    .as_builder()
                    .capacity(new_commitment_output_capacity)
                    .build();

                let settlement_output_capacity: u64 = settlement_output.capacity().unpack();
                let new_settlement_output_capacity = checked_add_u64(
                    settlement_output_capacity,
                    commitment_cell.output.capacity.value(),
                    "settlement output capacity",
                )
                .and_then(|amount| {
                    checked_sub_u64(
                        amount,
                        new_commitment_output_capacity,
                        "settlement output capacity",
                    )
                })?;
                settlement_output = settlement_output
                    .as_builder()
                    .capacity(new_settlement_output_capacity)
                    .build();

                checked_add_u64(
                    new_settlement_output_capacity,
                    new_commitment_output_capacity,
                    "outputs capacity",
                )?
            }
        } else {
            checked_add_u64(
                settlement_output_occupied_capacity,
                commitment_cell.output.capacity.value(),
                "outputs capacity",
            )?
        };

        if !two_parties_all_settled {
            tx_builder = tx_builder
                .output(new_commitment_output.clone())
                .output_data(new_commitment_output_data.clone());
        }

        tx_builder = tx_builder
            .output(settlement_output.clone())
            .output_data(settlement_output_data.clone())
            .output(change_output.clone())
            .output_data(Bytes::default())
            .witness(witness_for_commitment_cell.pack())
            .witness(placeholder_witness_for_change.as_bytes().pack());

        // TODO: move it to config or use https://github.com/nervosnetwork/ckb/pull/4477
        let fee_calculator = FeeCalculator::new(1000);
        // use two inputs as the maximum fee provider cell inputs
        let fee = fee_calculator.fee(tx_size_with_extra_inputs(&tx_builder, 2)?);

        let change_output_occupied_capacity = change_output
            .occupied_capacity(Capacity::shannons(0))
            .map_err(|err| {
                ArithmeticError::new(format!(
                    "change output occupied capacity calculation failed: {}",
                    err
                ))
            })?
            .as_u64();
        let min_total_capacity = checked_add_u64(
            change_output_occupied_capacity,
            outputs_capacity,
            "settlement min capacity",
        )
        .and_then(|amount| checked_add_u64(amount, fee, "settlement min capacity"))?;
        let mut query = CellQueryOptions::new_lock(fee_provider_lock_script);
        query.script_search_mode = Some(SearchMode::Exact);
        query.secondary_script_len_range = Some(ValueRangeOption::new_exact(0));
        query.data_len_range = Some(ValueRangeOption::new_exact(0));
        query.min_total_capacity = min_total_capacity;
        let (cells, _total_capacity) = cell_collector.collect_live_cells(&query, false)?;
        let mut inputs_capacity = commitment_cell.output.capacity.value();
        let since = if unlock.unlock_type < 0xFE && !unlock.with_preimage {
            Since::new(SinceType::Timestamp, current_time / 1000, false).value()
        } else {
            0
        };
        for cell in cells {
            let input_capacity: u64 = cell.output.capacity().unpack();
            inputs_capacity = checked_add_u64(
                inputs_capacity,
                input_capacity,
                "settlement inputs capacity",
            )?;
            tx_builder = tx_builder.input(
                CellInput::new_builder()
                    .previous_output(cell.out_point)
                    .since(since)
                    .build(),
            );
            let tx_size =
                u64::try_from(tx_builder.clone().build().data().serialized_size_in_block())
                    .map_err(|_| {
                        ArithmeticError::new("transaction size does not fit into u64".to_string())
                    })?;
            let fee = fee_calculator.fee(tx_size);
            let required_capacity = checked_add_u64(
                change_output_occupied_capacity,
                outputs_capacity,
                "settlement required capacity",
            )
            .and_then(|amount| checked_add_u64(amount, fee, "settlement required capacity"))?;
            if inputs_capacity >= required_capacity {
                let new_change_output = change_output
                    .as_builder()
                    .capacity(
                        checked_sub_u64(inputs_capacity, outputs_capacity, "change capacity")
                            .and_then(|amount| checked_sub_u64(amount, fee, "change capacity"))?,
                    )
                    .build();
                let outputs = if two_parties_all_settled {
                    vec![settlement_output, new_change_output]
                } else {
                    vec![new_commitment_output, settlement_output, new_change_output]
                };
                let outputs_data = if two_parties_all_settled {
                    vec![settlement_output_data, Bytes::default()]
                } else {
                    vec![
                        new_commitment_output_data,
                        settlement_output_data,
                        Bytes::default(),
                    ]
                };
                let tx = tx_builder
                    .set_outputs(outputs)
                    .set_outputs_data(outputs_data)
                    .build();
                let tx = sign_tx_with_settlement(tx, signer, unlock_key.0, unlock.with_preimage)?;
                return Ok(Some(tx));
            }
        }

        Err(Box::new(RpcError::Other(anyhow!("Not enough capacity"))))
    }
}

fn sign_tx(
    tx: TransactionView,
    signer: &LocalSigner,
) -> Result<TransactionView, Box<dyn std::error::Error>> {
    let tx = tx.data();
    let witness = tx.witnesses().get(1).expect("get witness at index 1");
    let mut blake2b = new_blake2b();
    blake2b.update(tx.calc_tx_hash().as_slice());
    blake2b.update(&(witness.item_count() as u64).to_le_bytes());
    blake2b.update(&witness.raw_data());
    let mut message = [0u8; 32];
    blake2b.finalize(&mut message);

    let signature_bytes = signer.sign_recoverable(&message);

    let witness = WitnessArgs::new_builder()
        .lock(Some(ckb_types::bytes::Bytes::from(signature_bytes.to_vec())).pack())
        .build();
    let witnesses = vec![
        tx.witnesses().get(0).expect("get witness at index 0"),
        witness.as_bytes().pack(),
    ];

    Ok(tx.as_advanced_builder().set_witnesses(witnesses).build())
}

fn sign_tx_with_settlement(
    tx: TransactionView,
    change_signer: &LocalSigner,
    settlement_secret_key: SecretKey,
    with_preimage: bool,
) -> Result<TransactionView, Box<dyn std::error::Error>> {
    let tx = tx.data().into_view();

    let message = compute_tx_message(&tx);
    let secp256k1_message = Message::from_digest_slice(&message)?;
    let signature = SECP256K1.sign_ecdsa_recoverable(&secp256k1_message, &settlement_secret_key);
    let (recov_id, data) = signature.serialize_compact();
    let mut signature_bytes = [0u8; 65];
    signature_bytes[0..64].copy_from_slice(&data[0..64]);
    signature_bytes[64] = i32::from(recov_id) as u8;
    let mut settlement_witness = tx
        .witnesses()
        .get(0)
        .expect("get witness at index 0")
        .raw_data()
        .to_vec();
    if with_preimage {
        let start = checked_sub_usize(settlement_witness.len(), 97, "settlement witness length")?;
        let end = checked_sub_usize(settlement_witness.len(), 32, "settlement witness length")?;
        settlement_witness.splice(start..end, signature_bytes);
    } else {
        let start = checked_sub_usize(settlement_witness.len(), 65, "settlement witness length")?;
        settlement_witness.splice(start.., signature_bytes);
    }

    let witness = tx.witnesses().get(1).expect("get witness at index 1");
    let mut blake2b = new_blake2b();
    blake2b.update(tx.hash().as_slice());
    blake2b.update(&(witness.item_count() as u64).to_le_bytes());
    blake2b.update(&witness.raw_data());
    let mut message = [0u8; 32];
    blake2b.finalize(&mut message);
    let signature_bytes = change_signer.sign_recoverable(&message);
    let change_witness = WitnessArgs::new_builder()
        .lock(Some(ckb_types::bytes::Bytes::from(signature_bytes.to_vec())).pack())
        .build()
        .as_bytes();

    let witnesses = vec![settlement_witness.pack(), change_witness.pack()];

    Ok(tx.as_advanced_builder().set_witnesses(witnesses).build())
}

#[derive(Debug)]
struct SettlementWitness {
    pending_htlc_count: usize,
    pending_htlcs: Vec<Htlc>,
    settlement_remote_pubkey_hash: [u8; 20],
    settlement_remote_amount: u128,
    settlement_local_pubkey_hash: [u8; 20],
    settlement_local_amount: u128,
    unlocks: Vec<Unlock>,
}

struct WitnessReader<'a> {
    remaining: &'a [u8],
}

impl<'a> WitnessReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }

    fn remaining(&self) -> &'a [u8] {
        self.remaining
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.remaining.len() < len {
            return None;
        }
        let (value, remaining) = self.remaining.split_at(len);
        self.remaining = remaining;
        Some(value)
    }

    fn take_u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn take_array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn take_u128_le(&mut self) -> Option<u128> {
        Some(u128::from_le_bytes(self.take_array()?))
    }
}

#[derive(Debug)]
struct Htlc {
    htlc_type: u8,
    payment_amount: u128,
    payment_hash: [u8; 20],
    remote_htlc_pubkey_hash: [u8; 20],
    local_htlc_pubkey_hash: [u8; 20],
    htlc_expiry: u64,
}

impl Htlc {
    pub fn build_from_witness(witness: &[u8]) -> Self {
        let htlc_type = witness[0];
        let payment_amount = u128::from_le_bytes(witness[1..17].try_into().unwrap());
        let payment_hash = witness[17..37].try_into().unwrap();
        let remote_htlc_pubkey_hash = witness[37..57].try_into().unwrap();
        let local_htlc_pubkey_hash = witness[57..77].try_into().unwrap();
        let htlc_expiry = u64::from_le_bytes(witness[77..].try_into().unwrap());
        Self {
            htlc_type,
            payment_amount,
            payment_hash,
            remote_htlc_pubkey_hash,
            local_htlc_pubkey_hash,
            htlc_expiry,
        }
    }

    pub fn to_witness(&self) -> Vec<u8> {
        let mut vec = Vec::new();
        vec.push(self.htlc_type);
        vec.extend_from_slice(&self.payment_amount.to_le_bytes());
        vec.extend_from_slice(&self.payment_hash);
        vec.extend_from_slice(&self.remote_htlc_pubkey_hash);
        vec.extend_from_slice(&self.local_htlc_pubkey_hash);
        vec.extend_from_slice(&self.htlc_expiry.to_le_bytes());
        vec
    }

    pub fn is_offered(&self) -> bool {
        self.htlc_type & 0b0000001 == 0
    }

    pub fn absolute_expiry(&self) -> Option<u64> {
        let since = Since::from_raw_value(self.htlc_expiry);
        if since.is_absolute() {
            match since.extract_metric() {
                Some((SinceType::Timestamp, expiry)) => {
                    checked_mul_u64(expiry, 1000, "HTLC timestamp expiry").ok()
                }
                _ => None,
            }
        } else {
            None
        }
    }

    pub fn find_matched_private_key<'a>(
        &self,
        settlement_data: &'a SettlementData,
        with_preimage: bool,
    ) -> Option<&'a Privkey> {
        settlement_data.tlcs.iter().find_map(|settlement_tlc| {
            let payment_hash_matches = settlement_tlc
                .payment_hash
                .as_ref()
                .starts_with(&self.payment_hash);
            let pubkey_hash_matches = match (self.is_offered(), with_preimage) {
                (true, true) | (false, false) => {
                    self.remote_htlc_pubkey_hash == settlement_tlc_local_pubkey_hash(settlement_tlc)
                }
                _ => {
                    self.local_htlc_pubkey_hash == settlement_tlc_local_pubkey_hash(settlement_tlc)
                }
            };
            (payment_hash_matches && pubkey_hash_matches).then_some(&settlement_tlc.local_key)
        })
    }
}

#[derive(Debug, Clone)]
struct Unlock {
    unlock_type: u8,
    with_preimage: bool,
    signature: [u8; 65],
    preimage: Option<Hash256>,
}

impl Unlock {
    pub fn build_from_witness(witness: &[u8]) -> Option<Self> {
        if witness.len() < 67 {
            return None;
        }
        let unlock_type = witness[0];
        let with_preimage = witness[1] == 1;
        if with_preimage && witness.len() < 99 {
            return None;
        }
        let signature = witness[2..67].try_into().unwrap();
        let preimage = if with_preimage {
            let preimage: [u8; 32] = witness[67..99].try_into().unwrap();
            Some(preimage.into())
        } else {
            None
        };
        Some(Self {
            unlock_type,
            with_preimage,
            signature,
            preimage,
        })
    }

    pub fn to_witness(&self) -> Vec<u8> {
        let mut vec = Vec::new();
        vec.push(self.unlock_type);
        vec.push(if self.with_preimage { 1 } else { 0 });
        vec.extend_from_slice(&self.signature);
        if self.with_preimage {
            vec.extend_from_slice(self.preimage.unwrap().as_ref());
        }
        vec
    }

    fn witness_len(&self) -> usize {
        if self.with_preimage {
            99
        } else {
            67
        }
    }
}

impl SettlementWitness {
    pub fn build_from_witness(witness: &[u8]) -> Option<Self> {
        let mut reader = WitnessReader::new(witness);
        let _unlock_count = reader.take_u8()?;
        let pending_htlc_count = reader.take_u8()? as usize;

        let mut pending_htlcs = Vec::with_capacity(pending_htlc_count);
        for _ in 0..pending_htlc_count {
            pending_htlcs.push(Htlc::build_from_witness(reader.take(85)?));
        }

        let settlement_remote_pubkey_hash = reader.take_array::<20>()?;
        let settlement_remote_amount = reader.take_u128_le()?;
        let settlement_local_pubkey_hash = reader.take_array::<20>()?;
        let settlement_local_amount = reader.take_u128_le()?;

        let mut unlocks = Vec::new();
        while !reader.is_empty() {
            let unlock = Unlock::build_from_witness(reader.remaining())?;
            reader.take(unlock.witness_len())?;
            unlocks.push(unlock);
        }

        Some(Self {
            pending_htlc_count,
            pending_htlcs,
            settlement_remote_pubkey_hash,
            settlement_remote_amount,
            settlement_local_pubkey_hash,
            settlement_local_amount,
            unlocks,
        })
    }

    // update for next settlement, return false if the unlocks are not valid
    pub fn update(&mut self) -> bool {
        let mut settled_htlcs = Vec::new();
        for unlock in self.unlocks.drain(0..) {
            match unlock.unlock_type {
                0xFF => {
                    self.settlement_local_amount = 0;
                    self.settlement_local_pubkey_hash = [0; 20];
                }
                0xFE => {
                    self.settlement_remote_amount = 0;
                    self.settlement_remote_pubkey_hash = [0; 20];
                }
                i if i < self.pending_htlc_count as u8 => {
                    settled_htlcs.push(i);
                }
                _ => return false,
            }
        }
        if !settled_htlcs.is_empty() {
            self.pending_htlc_count -= settled_htlcs.len();
            let mut current_index = 0;
            self.pending_htlcs.retain(|_| {
                let is_settled = settled_htlcs.contains(&current_index);
                current_index += 1;
                !is_settled
            });
        }
        true
    }

    pub fn to_witness(&self) -> Vec<u8> {
        let mut vec = Vec::new();
        vec.push(self.pending_htlc_count as u8);
        for htlc in &self.pending_htlcs {
            vec.extend_from_slice(&htlc.to_witness());
        }
        vec.extend_from_slice(&self.settlement_remote_pubkey_hash);
        vec.extend_from_slice(&self.settlement_remote_amount.to_le_bytes());
        vec.extend_from_slice(&self.settlement_local_pubkey_hash);
        vec.extend_from_slice(&self.settlement_local_amount.to_le_bytes());
        vec
    }
}

// Calculate the product of delay_epoch and a fraction
fn mul(
    delay: EpochNumberWithFraction,
    numerator: u64,
    denominator: u64,
) -> Option<EpochNumberWithFraction> {
    let delay_units = checked_mul_u64(delay.number(), delay.length(), "delay epoch units")
        .and_then(|amount| checked_add_u64(amount, delay.index(), "delay epoch units"))
        .ok()?;
    let full_numerator = checked_mul_u64(numerator, delay_units, "delay epoch numerator").ok()?;
    let new_denominator =
        checked_mul_u64(denominator, delay.length(), "delay epoch denominator").ok()?;
    if new_denominator == 0 {
        return None;
    }
    let new_integer = full_numerator / new_denominator;
    let new_numerator = full_numerator % new_denominator;

    // normalize the fraction (max epoch length is 1800)
    let scale_factor = if new_denominator > 1800 {
        checked_add_u64(new_denominator / 1800, 1, "delay epoch scale factor").ok()?
    } else {
        1
    };

    Some(EpochNumberWithFraction::new(
        new_integer,
        new_numerator / scale_factor,
        new_denominator / scale_factor,
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ckb_types::{core::ScriptHashType, packed::Byte32, prelude::*};

    use crate::fiber::onchain_tlc_reconcile::StoredOnChainTlcSettlement;

    use super::*;

    #[derive(Default)]
    struct TestWatchtowerStore {
        settlements: Mutex<Vec<(Hash256, TLCId, OnChainTlcSettlement)>>,
        preimages: Mutex<Vec<(NodeId, Hash256, Hash256)>>,
    }

    impl TestWatchtowerStore {
        fn settled_tlcs(&self) -> Vec<(Hash256, [u8; 20])> {
            self.settlements
                .lock()
                .expect("lock poisoned")
                .iter()
                .map(|(channel_id, _, settlement)| {
                    let prefix = settlement.payment_hash.as_ref()[..20]
                        .try_into()
                        .expect("payment hash prefix");
                    (*channel_id, prefix)
                })
                .collect()
        }

        fn settlements(&self) -> Vec<(Hash256, TLCId, OnChainTlcSettlement)> {
            self.settlements.lock().expect("lock poisoned").clone()
        }
    }

    impl WatchtowerStore for TestWatchtowerStore {
        fn get_watch_channels_with_nodes(&self) -> Vec<(NodeId, ChannelData)> {
            vec![]
        }

        fn insert_watch_channel(
            &self,
            _node_id: NodeId,
            _channel_id: Hash256,
            _funding_udt_type_script: Option<Script>,
            _local_settlement_key: Privkey,
            _remote_settlement_key: Pubkey,
            _local_funding_pubkey: Pubkey,
            _remote_funding_pubkey: Pubkey,
            _settlement_data: SettlementData,
        ) {
        }

        fn remove_watch_channel(&self, _node_id: NodeId, _channel_id: Hash256) {}

        fn update_revocation(
            &self,
            _node_id: NodeId,
            _channel_id: Hash256,
            _revocation_data: RevocationData,
            _remote_settlement_data: SettlementData,
        ) {
        }

        fn update_pending_remote_settlement(
            &self,
            _node_id: NodeId,
            _channel_id: Hash256,
            _pending_remote_settlement_data: SettlementData,
        ) {
        }

        fn update_local_settlement(
            &self,
            _node_id: NodeId,
            _channel_id: Hash256,
            _local_settlement_data: SettlementData,
        ) {
        }

        fn insert_watch_preimage(&self, node_id: NodeId, payment_hash: Hash256, preimage: Hash256) {
            self.preimages
                .lock()
                .expect("lock poisoned")
                .push((node_id, payment_hash, preimage));
        }

        fn remove_watch_preimage(&self, _node_id: NodeId, _payment_hash: Hash256) {}

        fn get_watch_preimage(&self, node_id: &NodeId, payment_hash: &Hash256) -> Option<Hash256> {
            self.preimages
                .lock()
                .expect("lock poisoned")
                .iter()
                .find_map(|(stored_node_id, stored_payment_hash, preimage)| {
                    (stored_node_id == node_id && stored_payment_hash == payment_hash)
                        .then_some(*preimage)
                })
        }

        fn insert_onchain_tlc_settlement(
            &self,
            channel_id: &Hash256,
            tlc_id: TLCId,
            settlement: OnChainTlcSettlement,
        ) {
            self.settlements
                .lock()
                .expect("lock poisoned")
                .push((*channel_id, tlc_id, settlement));
        }

        fn get_onchain_tlc_settlement(
            &self,
            channel_id: &Hash256,
            tlc_id: TLCId,
            _payment_hash: &Hash256,
        ) -> Option<StoredOnChainTlcSettlement> {
            self.settlements
                .lock()
                .expect("lock poisoned")
                .iter()
                .find_map(|(id, stored_tlc_id, settlement)| {
                    (id == channel_id && *stored_tlc_id == tlc_id)
                        .then(|| StoredOnChainTlcSettlement::Exact(settlement.clone()))
                })
        }
    }

    fn commitment_lock_prefix() -> Script {
        Script::new_builder()
            .code_hash(Byte32::from([1u8; 32]))
            .hash_type(ScriptHashType::Type)
            .args([2u8; 36].to_vec().pack())
            .build()
    }

    fn settlement_lock(prefix: &Script) -> Script {
        let mut args = prefix.args().raw_data().to_vec();
        args.extend_from_slice(&[3u8; 20]);
        args.push(1);
        prefix.clone().as_builder().args(args.pack()).build()
    }

    fn settlement_witness(payment_hash: [u8; 20]) -> Vec<u8> {
        settlement_witness_with_unlock(
            payment_hash,
            Unlock {
                unlock_type: 0,
                with_preimage: false,
                signature: [0u8; 65],
                preimage: None,
            },
        )
    }

    fn test_htlc(payment_hash: [u8; 20]) -> Htlc {
        Htlc {
            htlc_type: 0,
            payment_amount: 1_000,
            payment_hash,
            remote_htlc_pubkey_hash: [4u8; 20],
            local_htlc_pubkey_hash: [5u8; 20],
            htlc_expiry: 0,
        }
    }

    fn tracked_tlc(payment_hash_prefix: [u8; 20], tlc_id: u64) -> TrackedSettlementTlc {
        let mut payment_hash = [0u8; 32];
        payment_hash[..20].copy_from_slice(&payment_hash_prefix);
        payment_hash[24..].copy_from_slice(&tlc_id.to_be_bytes());
        TrackedSettlementTlc {
            tlc_id: TLCId::Offered(tlc_id),
            payment_hash: payment_hash.into(),
            hash_algorithm: HashAlgorithm::CkbHash,
            witness: test_htlc(payment_hash_prefix).to_witness(),
        }
    }

    fn watched_outpoints(
        outpoint: OutPoint,
        payment_hashes: &[[u8; 20]],
    ) -> HashMap<OutPoint, Vec<TrackedSettlementTlc>> {
        HashMap::from([(
            outpoint,
            payment_hashes
                .iter()
                .enumerate()
                .map(|(index, payment_hash)| tracked_tlc(*payment_hash, index as u64))
                .collect(),
        )])
    }

    fn settlement_witness_with_unlock(payment_hash: [u8; 20], unlock: Unlock) -> Vec<u8> {
        settlement_witness_with_unlocks(&[payment_hash], vec![unlock])
    }

    fn settlement_witness_with_unlocks(
        payment_hashes: &[[u8; 20]],
        unlocks: Vec<Unlock>,
    ) -> Vec<u8> {
        let settlement_witness = SettlementWitness {
            pending_htlc_count: payment_hashes.len(),
            pending_htlcs: payment_hashes
                .iter()
                .map(|payment_hash| test_htlc(*payment_hash))
                .collect(),
            settlement_remote_pubkey_hash: [6u8; 20],
            settlement_remote_amount: 2_000,
            settlement_local_pubkey_hash: [7u8; 20],
            settlement_local_amount: 3_000,
            unlocks: vec![],
        };

        let mut witness = [
            XUDT_COMPATIBLE_WITNESS.as_slice(),
            &[unlocks.len() as u8],
            settlement_witness.to_witness().as_slice(),
        ]
        .concat();
        for unlock in unlocks {
            witness.extend_from_slice(&unlock.to_witness());
        }
        witness
    }

    fn settlement_witness_final_party_unlock(
        payment_hashes: [[u8; 20]; 2],
        unlock_type: u8,
    ) -> Vec<u8> {
        let settlement_witness = SettlementWitness {
            pending_htlc_count: 2,
            pending_htlcs: payment_hashes.into_iter().map(test_htlc).collect(),
            settlement_remote_pubkey_hash: [6u8; 20],
            settlement_remote_amount: 2_000,
            settlement_local_pubkey_hash: [7u8; 20],
            settlement_local_amount: 3_000,
            unlocks: vec![],
        };
        let unlock = Unlock {
            unlock_type,
            with_preimage: false,
            signature: [0u8; 65],
            preimage: None,
        };

        [
            XUDT_COMPATIBLE_WITNESS.as_slice(),
            &[0x01],
            settlement_witness.to_witness().as_slice(),
            unlock.to_witness().as_slice(),
        ]
        .concat()
    }

    fn tx_with_input_output_and_witness(
        input_out_point: OutPoint,
        output_lock: Script,
        witness: Option<Vec<u8>>,
    ) -> Transaction {
        tx_with_inputs_outputs_and_witnesses(
            vec![input_out_point],
            vec![output_lock],
            witness.into_iter().collect(),
        )
    }

    fn tx_with_inputs_outputs_and_witnesses(
        input_out_points: Vec<OutPoint>,
        output_locks: Vec<Script>,
        witnesses: Vec<Vec<u8>>,
    ) -> Transaction {
        let mut tx_builder = Transaction::default().as_advanced_builder();
        for input_out_point in input_out_points {
            tx_builder = tx_builder.input(
                CellInput::new_builder()
                    .previous_output(input_out_point)
                    .build(),
            );
        }
        for output_lock in output_locks {
            tx_builder = tx_builder
                .output(CellOutput::new_builder().lock(output_lock).build())
                .output_data(Bytes::default());
        }
        for witness in witnesses {
            tx_builder = tx_builder.witness(witness.pack());
        }
        tx_builder.build().data()
    }

    #[test]
    fn settlement_builder_rejects_preimage_for_different_full_hash() {
        let self_node_id = NodeId::local();
        let preimage: Hash256 = [11u8; 32].into();
        let stored_payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
        let mut queried_payment_hash_bytes: [u8; 32] = stored_payment_hash
            .as_ref()
            .try_into()
            .expect("payment hash");
        queried_payment_hash_bytes[31] ^= 1;
        let queried_payment_hash: Hash256 = queried_payment_hash_bytes.into();
        let local_settlement_key = Privkey::from(&[1; 32]);
        let remote_settlement_key = Privkey::from(&[2; 32]).pubkey();
        let settlement_tlc = fiber_types::SettlementTlc {
            tlc_id: TLCId::Offered(0),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 1_000,
            payment_hash: queried_payment_hash,
            expiry: 60_000,
            local_key: Privkey::from(&[3; 32]),
            remote_key: Privkey::from(&[4; 32]).pubkey(),
        };
        let tracked_tlcs = vec![TrackedSettlementTlc {
            tlc_id: settlement_tlc.tlc_id.flip(),
            payment_hash: settlement_tlc.payment_hash,
            hash_algorithm: settlement_tlc.hash_algorithm,
            witness: settlement_tlc_to_witness(&settlement_tlc, false),
        }];
        let settlement_data = SettlementData {
            local_amount: 100_000_000_000,
            remote_amount: 100_000_000_000,
            tlcs: vec![settlement_tlc],
        };
        let channel_data = ChannelData {
            channel_id: [9u8; 32].into(),
            funding_udt_type_script: None,
            local_settlement_key: local_settlement_key.clone(),
            remote_settlement_key,
            local_funding_pubkey: Privkey::from(&[5; 32]).pubkey(),
            remote_funding_pubkey: Privkey::from(&[6; 32]).pubkey(),
            remote_settlement_data: settlement_data.clone(),
            pending_remote_settlement_data: settlement_data.clone(),
            local_settlement_data: settlement_data.clone(),
            revocation_data: None,
        };
        let settlement_witness = SettlementWitness::build_from_witness(
            &[
                &[0u8],
                settlement_data_to_witness(
                    &settlement_data,
                    false,
                    local_settlement_key,
                    remote_settlement_key,
                )
                .as_slice(),
            ]
            .concat(),
        )
        .expect("settlement witness");

        let delay_epoch = EpochNumberWithFraction::new(3, 0, 1);
        let since = Since::new(
            SinceType::EpochNumberWithFraction,
            delay_epoch.full_value(),
            true,
        )
        .value();
        let mut lock_args = vec![0u8; 20];
        lock_args.extend_from_slice(&since.to_le_bytes());
        lock_args.extend_from_slice(&0u64.to_be_bytes());
        let lock = Script::new_builder()
            .code_hash(Byte32::from([1u8; 32]))
            .hash_type(ScriptHashType::Type)
            .args(lock_args.pack())
            .build();
        let type_script = Script::new_builder()
            .code_hash(Byte32::from([2u8; 32]))
            .hash_type(ScriptHashType::Type)
            .build();
        let commitment_cell = Cell {
            output: CellOutput::new_builder()
                .capacity(200_000_000_000u64)
                .lock(lock)
                .type_(Some(type_script).pack())
                .build()
                .into(),
            // A safe builder returns before inspecting output data because no exact preimage
            // exists for the queried full hash. A prefix-only regression would reach this
            // deliberately invalid sentinel.
            output_data: Some(ckb_jsonrpc_types::JsonBytes::from_vec(Vec::new())),
            out_point: OutPoint::new(Byte32::from([3u8; 32]), 0).into(),
            block_number: 0u64.into(),
            tx_index: 0u32.into(),
        };
        let store = TestWatchtowerStore::default();
        store.insert_watch_preimage(self_node_id.clone(), stored_payment_hash, preimage);
        let signer = LocalSigner::new(SecretKey::from_slice(&[7u8; 32]).expect("secret key"));
        let mut cell_collector = new_default_cell_collector("http://127.0.0.1:8114");

        let result = build_settlement_tx(
            commitment_cell,
            EpochNumberWithFraction::new(10, 0, 1),
            EpochNumberWithFraction::new(10, 0, 1),
            0,
            &self_node_id,
            false,
            channel_data,
            Some(settlement_witness),
            &tracked_tlcs,
            &signer,
            &mut cell_collector,
            &store,
        );

        assert!(
            matches!(result, Ok(None)),
            "a preimage for a different full hash must not start settlement construction: {result:?}"
        );
    }

    #[test]
    fn unrelated_same_prefix_tx_does_not_pollute_store() {
        let lock_prefix = commitment_lock_prefix();
        let channel_id: Hash256 = [9u8; 32].into();
        let payment_hash = [42u8; 20];
        let attacker_tx = tx_with_input_output_and_witness(
            OutPoint::new([8u8; 32].pack(), 0),
            settlement_lock(&lock_prefix),
            Some(settlement_witness(payment_hash)),
        );
        let mut watched_outpoints =
            watched_outpoints(OutPoint::new([7u8; 32].pack(), 0), &[[43u8; 20]]);
        let mut processed_tx_hashes = HashSet::new();
        let store = TestWatchtowerStore::default();
        let self_node_id = NodeId::local();

        let processed = process_watched_settlement_tx(
            &attacker_tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        );

        assert!(processed.is_none());
        assert!(store.settled_tlcs().is_empty());
        assert_eq!(watched_outpoints.len(), 1);
    }

    #[test]
    fn watched_non_first_input_does_not_parse_unrelated_witness_zero() {
        let lock_prefix = commitment_lock_prefix();
        let channel_id: Hash256 = [9u8; 32].into();
        let malicious_payment_hash = [42u8; 20];
        let watched_out_point = OutPoint::new([7u8; 32].pack(), 0);
        let fee_out_point = OutPoint::new([8u8; 32].pack(), 0);
        let tx = tx_with_inputs_outputs_and_witnesses(
            vec![fee_out_point, watched_out_point.clone()],
            vec![settlement_lock(&lock_prefix)],
            vec![settlement_witness(malicious_payment_hash), vec![]],
        );
        let mut watched_outpoints = watched_outpoints(watched_out_point, &[malicious_payment_hash]);
        let mut processed_tx_hashes = HashSet::new();
        let store = TestWatchtowerStore::default();
        let self_node_id = NodeId::local();

        let processed = process_watched_settlement_tx(
            &tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        );

        assert_eq!(processed, None);
        assert!(store.settled_tlcs().is_empty());
    }

    #[test]
    fn watched_settlement_tx_updates_store_and_tracks_next_outpoint() {
        let lock_prefix = commitment_lock_prefix();
        let channel_id: Hash256 = [9u8; 32].into();
        let payment_hash = [42u8; 20];
        let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
        let tx = tx_with_input_output_and_witness(
            first_commitment_out_point.clone(),
            settlement_lock(&lock_prefix),
            Some(settlement_witness(payment_hash)),
        );
        let tracked = tracked_tlc(payment_hash, 0);
        let mut watched_outpoints =
            HashMap::from([(first_commitment_out_point, vec![tracked.clone()])]);
        let mut processed_tx_hashes = HashSet::new();
        let store = TestWatchtowerStore::default();
        let self_node_id = NodeId::local();

        let processed = process_watched_settlement_tx(
            &tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        );

        assert_eq!(processed, Some(0));
        assert_eq!(store.settled_tlcs(), vec![(channel_id, payment_hash)]);
        assert_eq!(
            store.settlements(),
            vec![(
                channel_id,
                TLCId::Offered(0),
                OnChainTlcSettlement {
                    payment_hash: tracked.payment_hash,
                    hash_algorithm: HashAlgorithm::CkbHash,
                    preimage: None,
                    tx_hash: tx.calc_tx_hash().into(),
                    tlc_index: 0,
                },
            )]
        );
        assert!(watched_outpoints.contains_key(&OutPoint::new(tx.calc_tx_hash(), 0)));
    }

    #[test]
    fn shared_prefix_unlock_index_maps_to_exact_tlc() {
        let lock_prefix = commitment_lock_prefix();
        let channel_id: Hash256 = [9u8; 32].into();
        let shared_prefix = [42u8; 20];
        let tracked = [tracked_tlc(shared_prefix, 0), tracked_tlc(shared_prefix, 1)];
        assert_ne!(tracked[0].payment_hash, tracked[1].payment_hash);

        let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
        let tx = tx_with_input_output_and_witness(
            first_commitment_out_point.clone(),
            settlement_lock(&lock_prefix),
            Some(settlement_witness_with_unlocks(
                &[shared_prefix, shared_prefix],
                vec![Unlock {
                    unlock_type: 1,
                    with_preimage: false,
                    signature: [0u8; 65],
                    preimage: None,
                }],
            )),
        );
        let mut watched_outpoints = HashMap::from([(first_commitment_out_point, tracked.to_vec())]);
        let mut processed_tx_hashes = HashSet::new();
        let store = TestWatchtowerStore::default();

        assert_eq!(
            process_watched_settlement_tx(
                &tx,
                &mut watched_outpoints,
                &mut processed_tx_hashes,
                &lock_prefix,
                &channel_id,
                &store,
                &NodeId::local(),
            ),
            Some(0)
        );

        let settlements = store.settlements();
        assert_eq!(settlements.len(), 1);
        assert_eq!(settlements[0].1, TLCId::Offered(1));
        assert_eq!(settlements[0].2.payment_hash, tracked[1].payment_hash);
        let remaining = watched_outpoints
            .get(&OutPoint::new(tx.calc_tx_hash(), 0))
            .expect("next settlement outpoint tracked");
        assert_eq!(remaining, &tracked[..1]);
    }

    #[test]
    fn final_party_unlock_marks_pending_tlcs_without_preimage() {
        let lock_prefix = commitment_lock_prefix();
        let channel_id: Hash256 = [9u8; 32].into();
        let payment_hashes = [[41u8; 20], [42u8; 20]];
        let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
        let tx = tx_with_input_output_and_witness(
            first_commitment_out_point.clone(),
            settlement_lock(&lock_prefix),
            Some(settlement_witness_final_party_unlock(payment_hashes, 0xFE)),
        );
        let tracked = [
            tracked_tlc(payment_hashes[0], 0),
            tracked_tlc(payment_hashes[1], 1),
        ];
        let mut watched_outpoints = HashMap::from([(first_commitment_out_point, tracked.to_vec())]);
        let mut processed_tx_hashes = HashSet::new();
        let store = TestWatchtowerStore::default();
        let self_node_id = NodeId::local();

        let processed = process_watched_settlement_tx(
            &tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        );

        assert_eq!(processed, Some(0));
        assert_eq!(
            store.settled_tlcs(),
            vec![
                (channel_id, payment_hashes[0]),
                (channel_id, payment_hashes[1])
            ]
        );
        assert_eq!(
            store.settlements(),
            vec![
                (
                    channel_id,
                    TLCId::Offered(0),
                    OnChainTlcSettlement {
                        payment_hash: tracked[0].payment_hash,
                        hash_algorithm: HashAlgorithm::CkbHash,
                        preimage: None,
                        tx_hash: tx.calc_tx_hash().into(),
                        tlc_index: 0,
                    },
                ),
                (
                    channel_id,
                    TLCId::Offered(1),
                    OnChainTlcSettlement {
                        payment_hash: tracked[1].payment_hash,
                        hash_algorithm: HashAlgorithm::CkbHash,
                        preimage: None,
                        tx_hash: tx.calc_tx_hash().into(),
                        tlc_index: 1,
                    },
                ),
            ]
        );
    }

    #[test]
    fn watched_tx_does_not_track_extra_same_prefix_output() {
        let lock_prefix = commitment_lock_prefix();
        let channel_id: Hash256 = [9u8; 32].into();
        let payment_hash = [42u8; 20];
        let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
        let tx = tx_with_inputs_outputs_and_witnesses(
            vec![first_commitment_out_point.clone()],
            vec![settlement_lock(&lock_prefix), settlement_lock(&lock_prefix)],
            vec![settlement_witness(payment_hash)],
        );
        let extra_same_prefix_out_point = OutPoint::new(tx.calc_tx_hash(), 1);
        let mut watched_outpoints = watched_outpoints(first_commitment_out_point, &[payment_hash]);
        let mut processed_tx_hashes = HashSet::new();
        let store = TestWatchtowerStore::default();
        let self_node_id = NodeId::local();

        let processed = process_watched_settlement_tx(
            &tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        );

        assert_eq!(processed, Some(0));
        assert!(!watched_outpoints.contains_key(&extra_same_prefix_out_point));
    }

    #[test]
    fn asc_processing_handles_parent_before_child_without_candidate_cache() {
        let lock_prefix = commitment_lock_prefix();
        let channel_id: Hash256 = [9u8; 32].into();
        let parent_payment_hash = [42u8; 20];
        let child_payment_hash = [43u8; 20];
        let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
        let parent_tx = tx_with_input_output_and_witness(
            first_commitment_out_point.clone(),
            settlement_lock(&lock_prefix),
            Some(settlement_witness_with_unlocks(
                &[parent_payment_hash, child_payment_hash],
                vec![Unlock {
                    unlock_type: 0,
                    with_preimage: false,
                    signature: [0u8; 65],
                    preimage: None,
                }],
            )),
        );
        let child_tx = tx_with_input_output_and_witness(
            OutPoint::new(parent_tx.calc_tx_hash(), 0),
            settlement_lock(&lock_prefix),
            Some(settlement_witness(child_payment_hash)),
        );
        let mut watched_outpoints = watched_outpoints(
            first_commitment_out_point,
            &[parent_payment_hash, child_payment_hash],
        );
        let mut processed_tx_hashes = HashSet::new();
        let store = TestWatchtowerStore::default();
        let self_node_id = NodeId::local();

        assert_eq!(
            process_watched_settlement_tx(
                &parent_tx,
                &mut watched_outpoints,
                &mut processed_tx_hashes,
                &lock_prefix,
                &channel_id,
                &store,
                &self_node_id,
            ),
            Some(0)
        );
        assert_eq!(
            process_watched_settlement_tx(
                &child_tx,
                &mut watched_outpoints,
                &mut processed_tx_hashes,
                &lock_prefix,
                &channel_id,
                &store,
                &self_node_id,
            ),
            Some(0)
        );

        assert_eq!(
            store.settled_tlcs(),
            vec![
                (channel_id, parent_payment_hash),
                (channel_id, child_payment_hash)
            ]
        );
        let settlements = store.settlements();
        assert_eq!(settlements[0].1, TLCId::Offered(0));
        assert_eq!(settlements[1].1, TLCId::Offered(1));
        assert_eq!(settlements[1].2.tlc_index, 0);
    }

    #[test]
    fn one_pass_processing_misses_child_when_indexer_returns_child_before_parent() {
        let lock_prefix = commitment_lock_prefix();
        let channel_id: Hash256 = [9u8; 32].into();
        let parent_payment_hash = [42u8; 20];
        let child_payment_hash = [43u8; 20];
        let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
        let parent_tx = tx_with_input_output_and_witness(
            first_commitment_out_point.clone(),
            settlement_lock(&lock_prefix),
            Some(settlement_witness_with_unlocks(
                &[parent_payment_hash, child_payment_hash],
                vec![Unlock {
                    unlock_type: 0,
                    with_preimage: false,
                    signature: [0u8; 65],
                    preimage: None,
                }],
            )),
        );
        let child_tx = tx_with_input_output_and_witness(
            OutPoint::new(parent_tx.calc_tx_hash(), 0),
            settlement_lock(&lock_prefix),
            Some(settlement_witness(child_payment_hash)),
        );
        let mut watched_outpoints = watched_outpoints(
            first_commitment_out_point,
            &[parent_payment_hash, child_payment_hash],
        );
        let mut processed_tx_hashes = HashSet::new();
        let store = TestWatchtowerStore::default();
        let self_node_id = NodeId::local();

        // Simulate child-before-parent order: child is skipped on first pass
        assert_eq!(
            process_watched_settlement_tx(
                &child_tx,
                &mut watched_outpoints,
                &mut processed_tx_hashes,
                &lock_prefix,
                &channel_id,
                &store,
                &self_node_id,
            ),
            None,
            "child processed before parent should return None"
        );

        // Parent is processed, adding its output to watched_outpoints
        assert_eq!(
            process_watched_settlement_tx(
                &parent_tx,
                &mut watched_outpoints,
                &mut processed_tx_hashes,
                &lock_prefix,
                &channel_id,
                &store,
                &self_node_id,
            ),
            Some(0)
        );

        // With multi-pass retry, child is retried and now succeeds
        assert_eq!(
            process_watched_settlement_tx(
                &child_tx,
                &mut watched_outpoints,
                &mut processed_tx_hashes,
                &lock_prefix,
                &channel_id,
                &store,
                &self_node_id,
            ),
            Some(0),
            "child should succeed on retry after parent is processed"
        );

        assert_eq!(
            store.settled_tlcs(),
            vec![
                (channel_id, parent_payment_hash),
                (channel_id, child_payment_hash)
            ]
        );
        let settlements = store.settlements();
        assert_eq!(settlements[0].1, TLCId::Offered(0));
        assert_eq!(settlements[1].1, TLCId::Offered(1));
    }
}
