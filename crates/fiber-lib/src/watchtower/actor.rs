use anyhow::anyhow;
use ckb_hash::new_blake2b;
use ckb_jsonrpc_types::{Either, Status};
use ckb_sdk::{
    rpc::ckb_indexer::{Cell, CellType, Order, ScriptType, SearchKey, SearchMode, Tx},
    traits::{CellCollector, CellQueryOptions, DefaultCellCollector, ValueRangeOption},
    transaction::builder::FeeCalculator,
    util::blake160,
    CkbRpcClient, RpcError, Since, SinceType,
};
use ckb_types::{
    self,
    core::{Capacity, EpochNumberWithFraction, HeaderView, TransactionView},
    packed::{Bytes, CellInput, CellOutput, OutPoint, Script, Transaction, WitnessArgs},
    prelude::*,
};
use molecule::prelude::Entity;
use ractor::{Actor, ActorProcessingErr, ActorRef};
use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use strum::AsRefStr;
use tracing::{debug, error, info, warn};

use crate::{
    ckb::{
        config::{new_default_cell_collector, CKB_RPC_TIMEOUT},
        contracts::{get_cell_deps_sync, get_script_by_contract, Contract},
        CkbConfig,
    },
    fiber::{
        channel::{RevocationData, SettlementData, XUDT_COMPATIBLE_WITNESS},
        hash_algorithm::HashAlgorithm,
        types::{Hash256, NodeId, Privkey, Pubkey},
    },
    utils::tx::compute_tx_message,
    watchtower::ChannelData,
};

use super::WatchtowerStore;

pub const DEFAULT_WATCHTOWER_CHECK_INTERVAL_SECONDS: u64 = 60;

pub struct WatchtowerActor<S> {
    store: S,
    // a node_id represent the watchtower itself
    node_id: NodeId,
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
    secret_key: SecretKey,
}

#[async_trait::async_trait]
impl<S> Actor for WatchtowerActor<S>
where
    S: WatchtowerStore + Send + Sync + 'static,
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
        Ok(Self::State { config, secret_key })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        #[cfg(feature = "metrics")]
        let start = crate::now_timestamp_as_millis_u64();
        #[cfg(feature = "metrics")]
        let name = format!("fiber.watchtower_actor.{}", message.as_ref());
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
            WatchtowerMessage::PeriodicCheck => self.periodic_check(state),
        }
        #[cfg(feature = "metrics")]
        {
            let end = crate::now_timestamp_as_millis_u64();
            let elapsed = end - start;
            metrics::histogram!(name).record(elapsed as u32);
        }
        Ok(())
    }
}

impl<S> WatchtowerActor<S>
where
    S: WatchtowerStore,
{
    fn periodic_check(&self, state: &WatchtowerState) {
        let secret_key = state.secret_key;
        let rpc_url = state.config.rpc_url.clone();
        tokio::task::block_in_place(move || {
            let mut cell_collector = new_default_cell_collector(&rpc_url);

            for channel_data in self.store.get_watch_channels() {
                let ckb_client = CkbRpcClient::with_builder(&rpc_url, |builder| {
                    builder.timeout(CKB_RPC_TIMEOUT)
                })
                .expect("create ckb rpc client should not fail");
                let search_key = SearchKey {
                    script: channel_data.funding_tx_lock().into(),
                    script_type: ScriptType::Lock,
                    script_search_mode: Some(SearchMode::Exact),
                    with_data: None,
                    filter: None,
                    group_by_transaction: None,
                };
                // we need two parties' signatures to unlock the funding tx, so we can check the last one transaction only to see if it's an old version commitment tx
                match ckb_client.get_transactions(search_key, Order::Desc, 1u32.into(), None) {
                    Ok(txs) => {
                        if let Some(Tx::Ungrouped(tx)) = txs.objects.first() {
                            if matches!(tx.io_type, CellType::Input) {
                                match ckb_client.get_transaction(tx.tx_hash.clone()) {
                                    Ok(Some(tx_with_status)) => {
                                        if tx_with_status.tx_status.status != Status::Committed {
                                            error!("Cannot find the commitment tx: {:?}, status is {:?}, maybe ckb indexer bug?", tx_with_status.tx_status.status, tx.tx_hash);
                                        } else if let Some(tx) = tx_with_status.transaction {
                                            match tx.inner {
                                                Either::Left(tx) => {
                                                    let tx: Transaction = tx.inner.into();
                                                    if tx.raw().outputs().len() == 1 {
                                                        let first_commitment_tx_out_point =
                                                            OutPoint::new(tx.calc_tx_hash(), 0);
                                                        let output = tx
                                                            .raw()
                                                            .outputs()
                                                            .get(0)
                                                            .expect("get output 0 of tx");
                                                        let commitment_lock = output.lock();
                                                        let lock_args =
                                                            commitment_lock.args().raw_data();
                                                        let pub_key_hash: [u8; 20] = lock_args
                                                            [0..20]
                                                            .try_into()
                                                            .expect("checked length");
                                                        let commitment_number = u64::from_be_bytes(
                                                            lock_args[28..36]
                                                                .try_into()
                                                                .expect("u64 from slice"),
                                                        );

                                                        let x_only_aggregated_pubkey = channel_data
                                                            .x_only_aggregated_pubkey(false);
                                                        if blake160(&x_only_aggregated_pubkey).0
                                                            == pub_key_hash
                                                        {
                                                            match channel_data
                                                                .revocation_data
                                                                .clone()
                                                            {
                                                                Some(revocation_data)
                                                                    if revocation_data
                                                                        .commitment_number
                                                                        >= commitment_number =>
                                                                {
                                                                    match ckb_client.get_live_cell(
                                                                        first_commitment_tx_out_point
                                                                            .clone()
                                                                            .into(),
                                                                        false,
                                                                    ) {
                                                                        Ok(cell_with_status) => {
                                                                            if cell_with_status
                                                                                .status
                                                                                == "live"
                                                                            {
                                                                                warn!("Found an old version commitment tx submitted by remote: {:#x}", tx.calc_tx_hash());
                                                                                match build_revocation_tx(
                                                                                    first_commitment_tx_out_point,
                                                                                    revocation_data,
                                                                                    x_only_aggregated_pubkey,
                                                                                    secret_key,
                                                                                    &mut cell_collector,
                                                                                ) {
                                                                                    Ok(tx) => {
                                                                                        match ckb_client
                                                                                            .send_transaction(
                                                                                                tx.data()
                                                                                                    .into(),
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
                                                                        secret_key,
                                                                        &mut cell_collector,
                                                                        &self.store,
                                                                        self.node_id.clone(),
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
                                                                secret_key,
                                                                &mut cell_collector,
                                                                &self.store,
                                                                self.node_id.clone(),
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
                                            error!("Cannot find the commitment tx: {:?}, transaction is none, maybe ckb indexer bug?", tx.tx_hash);
                                        }
                                    }
                                    Ok(None) => {
                                        error!("Cannot find the commitment tx: {:?}, maybe ckb indexer bug?", tx.tx_hash);
                                    }
                                    Err(err) => {
                                        error!("Failed to get funding tx: {:?}", err);
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => {
                        error!("Failed to get transactions: {:?}", err);
                    }
                }
            }
        });
    }
}

fn build_revocation_tx(
    commitment_tx_out_point: OutPoint,
    revocation_data: RevocationData,
    x_only_aggregated_pubkey: [u8; 32],
    secret_key: SecretKey,
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

    let pubkey = PublicKey::from_secret_key(&Secp256k1::new(), &secret_key);
    let args = blake160(pubkey.serialize().as_ref());
    let fee_provider_lock_script = get_script_by_contract(Contract::Secp256k1Lock, args.as_bytes());

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
    let fee = fee_calculator.fee(
        tx_builder.clone().build().data().serialized_size_in_block() as u64
            + CellInput::TOTAL_SIZE as u64 * 2,
    );
    let min_total_capacity = change_output_occupied_capacity + fee;
    let mut query = CellQueryOptions::new_lock(fee_provider_lock_script);
    query.script_search_mode = Some(SearchMode::Exact);
    query.secondary_script_len_range = Some(ValueRangeOption::new_exact(0));
    query.data_len_range = Some(ValueRangeOption::new_exact(0));
    query.min_total_capacity = min_total_capacity;
    let (cells, _total_capacity) = cell_collector.collect_live_cells(&query, false)?;
    let mut inputs_capacity = 0u64;
    for cell in cells {
        let input_capacity: u64 = cell.output.capacity().unpack();
        inputs_capacity += input_capacity;
        tx_builder = tx_builder.input(
            CellInput::new_builder()
                .previous_output(cell.out_point)
                .build(),
        );
        let fee =
            fee_calculator.fee(tx_builder.clone().build().data().serialized_size_in_block() as u64);
        if inputs_capacity >= change_output_occupied_capacity + fee {
            let new_change_output = change_output
                .as_builder()
                .capacity((inputs_capacity - fee).pack())
                .build();
            let tx = tx_builder
                .set_outputs(vec![revocation_data.output, new_change_output])
                .build();

            let tx = sign_tx(tx, secret_key)?;
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
    secret_key: SecretKey,
    cell_collector: &mut DefaultCellCollector,
    store: &S,
    self_node_id: NodeId,
) {
    let lock_args = commitment_lock.args().raw_data();
    let script = commitment_lock
        .as_builder()
        .args(lock_args[0..36].to_vec().pack())
        .build();
    let search_key = SearchKey {
        script: script.into(),
        script_type: ScriptType::Lock,
        script_search_mode: Some(SearchMode::Prefix),
        with_data: None,
        filter: None,
        group_by_transaction: Some(true),
    };

    find_preimages(search_key.clone(), &ckb_client, store, self_node_id);

    let (current_epoch, current_time) = match ckb_client.get_tip_header() {
        Ok(tip_header) => match ckb_client.get_block_median_time(tip_header.hash.clone()) {
            Ok(Some(median_time)) => {
                let tip_header: HeaderView = tip_header.into();
                let epoch = tip_header.epoch();
                (epoch, median_time.value())
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
        },
        Err(err) => {
            error!("Failed to get tip header: {:?}", err);
            return;
        }
    };
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
                                            match tx.witnesses().get(0) {
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
                        for_remote,
                        channel_data.clone(),
                        settlement_witness,
                        secret_key,
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
                error!("Failed to get cells: {:?}", err);
            }
        }
    }
}

// find all on-chain transactions with the preimage and store them
fn find_preimages<S: WatchtowerStore>(
    search_key: SearchKey,
    ckb_client: &CkbRpcClient,
    store: &S,
    self_node_id: NodeId,
) {
    let mut after = None;
    loop {
        match ckb_client.get_transactions(
            search_key.clone(),
            Order::Desc,
            100u32.into(),
            after.clone(),
        ) {
            Ok(txs) => {
                if txs.objects.is_empty() {
                    break;
                }
                after = Some(txs.last_cursor.clone());
                for tx in txs.objects {
                    match ckb_client.get_transaction(tx.tx_hash()) {
                        Ok(Some(tx_with_status)) => {
                            if tx_with_status.tx_status.status != Status::Committed {
                                error!("Cannot find the tx: {:?}, status is {:?}, maybe ckb indexer bug?", tx_with_status.tx_status.status, tx.tx_hash());
                            } else if let Some(tx) = tx_with_status.transaction {
                                match tx.inner {
                                    Either::Left(tx) => {
                                        let tx: Transaction = tx.inner.into();
                                        match tx.witnesses().get(0) {
                                            Some(witness) => {
                                                let witness = witness.raw_data();
                                                if witness.len() > 18
                                                    && witness[0..16] == XUDT_COMPATIBLE_WITNESS
                                                {
                                                    if let Some(settlement_witness) =
                                                        SettlementWitness::build_from_witness(
                                                            &witness[16..],
                                                        )
                                                    {
                                                        for unlock in settlement_witness.unlocks {
                                                            if unlock.with_preimage
                                                                && unlock.unlock_type < 0xFE
                                                            {
                                                                if let Some(tlc) =
                                                                    settlement_witness
                                                                        .pending_htlcs
                                                                        .get(
                                                                            unlock.unlock_type
                                                                                as usize,
                                                                        )
                                                                {
                                                                    let preimage =
                                                                        unlock.preimage.unwrap();
                                                                    let payment_hash = tlc
                                                                        .hash_algorithm()
                                                                        .hash(preimage.as_ref());
                                                                    if payment_hash.starts_with(
                                                                        &tlc.payment_hash,
                                                                    ) {
                                                                        store
                                                                            .insert_watch_preimage(
                                                                                self_node_id
                                                                                    .clone(),
                                                                                payment_hash.into(),
                                                                                preimage,
                                                                            );
                                                                    } else {
                                                                        warn!("Found a preimage for payment hash: {:?}, but not match the tlc, tx hash: {:?}", payment_hash, tx.calc_tx_hash());
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            None => {
                                                warn!("Found a commitment tx, but the witnesses are empty: {:?}", tx.calc_tx_hash());
                                            }
                                        }
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
                                tx.tx_hash()
                            );
                        }
                        Err(err) => {
                            error!("Failed to get tx: {:?}", err);
                        }
                    }
                }
            }
            Err(err) => {
                error!("Failed to get transactions: {:?}", err);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_settlement_tx<S: WatchtowerStore>(
    commitment_cell: Cell,
    cell_header_epoch: EpochNumberWithFraction,
    current_epoch: EpochNumberWithFraction,
    current_time: u64,
    for_remote: bool,
    channel_data: ChannelData,
    settlement_witness: Option<SettlementWitness>,
    secret_key: SecretKey,
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
    let settlement_data = if for_remote {
        if channel_data
            .revocation_data
            .as_ref()
            .map(|r| r.commitment_number)
            .unwrap_or_default()
            == commitment_number - 1
        {
            channel_data.remote_settlement_data.clone()
        } else {
            channel_data.pending_remote_settlement_data.clone()
        }
    } else {
        channel_data.local_settlement_data.clone()
    };

    let pubkey = PublicKey::from_secret_key(&Secp256k1::new(), &secret_key);
    let args = blake160(pubkey.serialize().as_ref());
    let fee_provider_lock_script = get_script_by_contract(Contract::Secp256k1Lock, args.as_bytes());
    let change_output = CellOutput::new_builder()
        .lock(fee_provider_lock_script.clone())
        .build();

    let mut two_parties_all_settled = false;
    let (unlock, mut unlock_amount, unlock_key, new_settlement_witness) = match settlement_witness {
        Some(mut sw) => {
            if sw.update() {
                debug!("channel_data local_settlement_key pubkey hash: {:?}，sw settlement_remote_pubkey_hash: {:?}, sw settlement_local_pubkey_hash: {:?}, for_remote: {}",
                    channel_data.local_settlement_pubkey_hash(), sw.settlement_remote_pubkey_hash, sw.settlement_local_pubkey_hash, for_remote);
                if for_remote {
                    if sw.settlement_local_pubkey_hash
                        == channel_data.local_settlement_pubkey_hash()
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
                                        settlement_data.tlcs.iter().find_map(|settlement_tlc| {
                                            settlement_tlc
                                                .payment_hash
                                                .as_ref()
                                                .starts_with(&tlc.payment_hash)
                                                .then_some(&settlement_tlc.local_key)
                                        })
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
                                    settlement_data.tlcs.iter().find_map(|settlement_tlc| {
                                        settlement_tlc
                                            .payment_hash
                                            .as_ref()
                                            .starts_with(&tlc.payment_hash)
                                            .then_some(&settlement_tlc.local_key)
                                    })
                                {
                                    if let Some(preimage) = store.search_preimage(&tlc.payment_hash)
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
                                        pending_tlcs_count -= 1;
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
                    == channel_data.local_settlement_pubkey_hash()
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
                                    settlement_data.tlcs.iter().find_map(|settlement_tlc| {
                                        settlement_tlc
                                            .payment_hash
                                            .as_ref()
                                            .starts_with(&tlc.payment_hash)
                                            .then_some(&settlement_tlc.local_key)
                                    })
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
                                settlement_data.tlcs.iter().find_map(|settlement_tlc| {
                                    settlement_tlc
                                        .payment_hash
                                        .as_ref()
                                        .starts_with(&tlc.payment_hash)
                                        .then_some(&settlement_tlc.local_key)
                                })
                            {
                                if let Some(preimage) = store.search_preimage(&tlc.payment_hash) {
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
                                    pending_tlcs_count -= 1;
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
            let mut pending_tlcs_count = settlement_data.tlcs.len();
            let mut unlock_option = None;
            for (i, tlc) in settlement_data.tlcs.iter().enumerate() {
                match (tlc.tlc_id.is_offered(), for_remote) {
                    (true, true) | (false, false) => {
                        let delay = mul(delay_epoch, 2, 3);
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
                        let delay = mul(delay_epoch, 1, 3);
                        if cell_header_epoch.to_rational() + delay.to_rational()
                            <= current_epoch.to_rational()
                        {
                            if let Some(preimage) = store.get_watch_preimage(&tlc.payment_hash) {
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
                                pending_tlcs_count -= 1;
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
                    settlement_data.to_witness(
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
            .capacity(new_capacity.pack())
            .build();
        let settlement_output = CellOutput::new_builder()
            .lock(fee_provider_lock_script.clone())
            .capacity((unlock_amount as u64).pack())
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
                .previous_output(commitment_cell.out_point.clone().into())
                .since(since.pack())
                .build()
        } else {
            CellInput::new_builder()
                .previous_output(commitment_cell.out_point.clone().into())
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
        let fee = fee_calculator.fee(
            tx_builder.clone().build().data().serialized_size_in_block() as u64
                + CellInput::TOTAL_SIZE as u64 * 2,
        );
        let settlement_output_occupied_capacity = settlement_output
            .occupied_capacity(Capacity::shannons(0))
            .expect("capacity does not overflow")
            .as_u64();
        let min_total_capacity =
            capacity.saturating_sub(new_capacity + settlement_output_occupied_capacity + fee);
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
            inputs_capacity += input_capacity;
            tx_builder = tx_builder.input(
                CellInput::new_builder()
                    .previous_output(cell.out_point)
                    .since(since.pack())
                    .build(),
            );
            let fee = fee_calculator
                .fee(tx_builder.clone().build().data().serialized_size_in_block() as u64);
            if inputs_capacity >= new_capacity + settlement_output_occupied_capacity + fee {
                let adjusted_settlement_output = change_output
                    .as_builder()
                    .capacity((inputs_capacity - new_capacity - fee).pack())
                    .build();
                let outputs = if two_parties_all_settled {
                    vec![adjusted_settlement_output]
                } else {
                    vec![new_commitment_output, adjusted_settlement_output]
                };
                let tx = tx_builder.set_outputs(outputs).build();
                let tx =
                    sign_tx_with_settlement(tx, secret_key, unlock_key.0, unlock.with_preimage)?;
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
            .capacity(settlement_output_occupied_capacity.pack())
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
                .previous_output(commitment_cell.out_point.clone().into())
                .since(since.pack())
                .build()
        } else {
            CellInput::new_builder()
                .previous_output(commitment_cell.out_point.clone().into())
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
                    .expect("capacity does not overflow")
                    .as_u64();
                new_commitment_output = new_commitment_output
                    .as_builder()
                    .capacity(new_commitment_output_capacity.pack())
                    .build();

                let settlement_output_capacity: u64 = settlement_output.capacity().unpack();
                let new_settlement_output_capacity = settlement_output_capacity
                    + commitment_cell.output.capacity.value()
                    - new_commitment_output_capacity;
                settlement_output = settlement_output
                    .as_builder()
                    .capacity(new_settlement_output_capacity.pack())
                    .build();

                new_settlement_output_capacity + new_commitment_output_capacity
            }
        } else {
            settlement_output_occupied_capacity + commitment_cell.output.capacity.value()
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
        let fee = fee_calculator.fee(
            tx_builder.clone().build().data().serialized_size_in_block() as u64
                + CellInput::TOTAL_SIZE as u64 * 2,
        );

        let change_output_occupied_capacity = change_output
            .occupied_capacity(Capacity::shannons(0))
            .expect("capacity does not overflow")
            .as_u64();
        let min_total_capacity = change_output_occupied_capacity + outputs_capacity + fee;
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
            inputs_capacity += input_capacity;
            tx_builder = tx_builder.input(
                CellInput::new_builder()
                    .previous_output(cell.out_point)
                    .since(since.pack())
                    .build(),
            );
            let fee = fee_calculator
                .fee(tx_builder.clone().build().data().serialized_size_in_block() as u64);
            if inputs_capacity >= change_output_occupied_capacity + outputs_capacity + fee {
                let new_change_output = change_output
                    .as_builder()
                    .capacity((inputs_capacity - outputs_capacity - fee).pack())
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
                let tx =
                    sign_tx_with_settlement(tx, secret_key, unlock_key.0, unlock.with_preimage)?;
                return Ok(Some(tx));
            }
        }

        Err(Box::new(RpcError::Other(anyhow!("Not enough capacity"))))
    }
}

fn sign_tx(
    tx: TransactionView,
    secret_key: SecretKey,
) -> Result<TransactionView, Box<dyn std::error::Error>> {
    let tx = tx.data();
    let witness = tx.witnesses().get(1).expect("get witness at index 1");
    let mut blake2b = new_blake2b();
    blake2b.update(tx.calc_tx_hash().as_slice());
    blake2b.update(&(witness.item_count() as u64).to_le_bytes());
    blake2b.update(&witness.raw_data());
    let mut message = vec![0u8; 32];
    blake2b.finalize(&mut message);
    let secp256k1_message = Message::from_digest_slice(&message)?;
    let secp256k1 = Secp256k1::new();
    let signature = secp256k1.sign_ecdsa_recoverable(&secp256k1_message, &secret_key);
    let (recov_id, data) = signature.serialize_compact();
    let mut signature_bytes = [0u8; 65];
    signature_bytes[0..64].copy_from_slice(&data[0..64]);
    signature_bytes[64] = recov_id.to_i32() as u8;

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
    change_secret_key: SecretKey,
    settlement_secret_key: SecretKey,
    with_preimage: bool,
) -> Result<TransactionView, Box<dyn std::error::Error>> {
    let tx = tx.data().into_view();

    let message = compute_tx_message(&tx);
    let secp256k1_message = Message::from_digest_slice(&message)?;
    let secp256k1 = Secp256k1::new();
    let signature = secp256k1.sign_ecdsa_recoverable(&secp256k1_message, &settlement_secret_key);
    let (recov_id, data) = signature.serialize_compact();
    let mut signature_bytes = [0u8; 65];
    signature_bytes[0..64].copy_from_slice(&data[0..64]);
    signature_bytes[64] = recov_id.to_i32() as u8;
    let mut settlement_witness = tx
        .witnesses()
        .get(0)
        .expect("get witness at index 0")
        .raw_data()
        .to_vec();
    if with_preimage {
        settlement_witness.splice(
            settlement_witness.len() - 97..settlement_witness.len() - 32,
            signature_bytes,
        );
    } else {
        settlement_witness.splice(settlement_witness.len() - 65.., signature_bytes);
    }

    let witness = tx.witnesses().get(1).expect("get witness at index 1");
    let mut blake2b = new_blake2b();
    blake2b.update(tx.hash().as_slice());
    blake2b.update(&(witness.item_count() as u64).to_le_bytes());
    blake2b.update(&witness.raw_data());
    let mut message = vec![0u8; 32];
    blake2b.finalize(&mut message);
    let secp256k1_message = Message::from_digest_slice(&message)?;
    let secp256k1 = Secp256k1::new();
    let signature = secp256k1.sign_ecdsa_recoverable(&secp256k1_message, &change_secret_key);
    let (recov_id, data) = signature.serialize_compact();
    let mut signature_bytes = [0u8; 65];
    signature_bytes[0..64].copy_from_slice(&data[0..64]);
    signature_bytes[64] = recov_id.to_i32() as u8;
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

    pub fn hash_algorithm(&self) -> HashAlgorithm {
        if (self.htlc_type >> 1) & 0b0000001 == 0 {
            HashAlgorithm::CkbHash
        } else {
            HashAlgorithm::Sha256
        }
    }

    pub fn is_offered(&self) -> bool {
        self.htlc_type & 0b0000001 == 0
    }

    pub fn absolute_expiry(&self) -> Option<u64> {
        let since = Since::from_raw_value(self.htlc_expiry);
        if since.is_absolute() {
            match since.extract_metric() {
                Some((SinceType::Timestamp, expiry)) => Some(expiry * 1000),
                _ => None,
            }
        } else {
            None
        }
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
}

impl SettlementWitness {
    pub fn build_from_witness(witness: &[u8]) -> Option<Self> {
        let pending_htlc_count = witness[1] as usize;
        let pending_htlc_witness_len = 85 * pending_htlc_count;
        // 1 byte for unlock_count, 1 byte for pending_htlc_count, 72 bytes for settlement script
        if witness.len() < 1 + 1 + pending_htlc_witness_len + 72 {
            return None;
        }
        let pending_htlcs = (2..2 + pending_htlc_witness_len)
            .step_by(85)
            .map(|index| Htlc::build_from_witness(&witness[index..index + 85]))
            .collect();
        let settlement_remote_pubkey_hash = witness
            [2 + pending_htlc_witness_len..22 + pending_htlc_witness_len]
            .try_into()
            .unwrap();
        let settlement_remote_amount = u128::from_le_bytes(
            witness[22 + pending_htlc_witness_len..38 + pending_htlc_witness_len]
                .try_into()
                .unwrap(),
        );
        let settlement_local_pubkey_hash = witness
            [38 + pending_htlc_witness_len..58 + pending_htlc_witness_len]
            .try_into()
            .unwrap();
        let settlement_local_amount = u128::from_le_bytes(
            witness[58 + pending_htlc_witness_len..74 + pending_htlc_witness_len]
                .try_into()
                .unwrap(),
        );
        let mut unlocks = Vec::new();
        let mut unlock_type_index = 74 + pending_htlc_witness_len;
        while unlock_type_index < witness.len() {
            match Unlock::build_from_witness(&witness[unlock_type_index..]) {
                Some(unlock) => {
                    if unlock.with_preimage {
                        unlock_type_index += 99;
                    } else {
                        unlock_type_index += 67;
                    }
                    unlocks.push(unlock);
                    if unlock_type_index == witness.len() {
                        break;
                    }
                }
                None => {
                    return None;
                }
            }
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
) -> EpochNumberWithFraction {
    let full_numerator = numerator * (delay.number() * delay.length() + delay.index());
    let new_denominator = denominator * delay.length();
    let new_integer = full_numerator / new_denominator;
    let new_numerator = full_numerator % new_denominator;

    // normalize the fraction (max epoch length is 1800)
    let scale_factor = if new_denominator > 1800 {
        new_denominator / 1800 + 1
    } else {
        1
    };

    EpochNumberWithFraction::new(
        new_integer,
        new_numerator / scale_factor,
        new_denominator / scale_factor,
    )
}
