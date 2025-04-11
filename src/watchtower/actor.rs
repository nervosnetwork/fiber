use anyhow::anyhow;
use ckb_hash::{blake2b_256, new_blake2b};
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
use tracing::{debug, error, info, trace, warn};

use crate::{
    ckb::{
        contracts::{get_cell_deps, get_script_by_contract, Contract},
        CkbConfig,
    },
    fiber::{
        channel::{
            create_witness_for_commitment_cell,
            create_witness_for_commitment_cell_with_pending_tlcs, RevocationData, SettlementData,
            SettlementTlc, XUDT_COMPATIBLE_WITNESS,
        },
        hash_algorithm::HashAlgorithm,
        types::Hash256,
    },
    invoice::InvoiceStore,
    NetworkServiceEvent,
};

use super::WatchtowerStore;

pub const DEFAULT_WATCHTOWER_CHECK_INTERVAL_SECONDS: u64 = 60;

pub struct WatchtowerActor<S> {
    store: S,
}

impl<S: InvoiceStore + WatchtowerStore> WatchtowerActor<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

pub enum WatchtowerMessage {
    NetworkServiceEvent(NetworkServiceEvent),
    PeriodicCheck,
}

pub struct WatchtowerState {
    config: CkbConfig,
    secret_key: SecretKey,
}

#[ractor::async_trait]
impl<S> Actor for WatchtowerActor<S>
where
    S: InvoiceStore + WatchtowerStore + Send + Sync + 'static,
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
        match message {
            WatchtowerMessage::NetworkServiceEvent(event) => {
                trace!("Received NetworkServiceEvent: {:?}", event);
                match event {
                    NetworkServiceEvent::RemoteTxComplete(
                        _peer_id,
                        channel_id,
                        funding_tx_lock,
                        settlement_data,
                    ) => {
                        self.store.insert_watch_channel(
                            channel_id,
                            funding_tx_lock,
                            settlement_data,
                        );
                    }
                    NetworkServiceEvent::ChannelClosed(_peer_id, channel_id, _close_tx_hash) => {
                        self.store.remove_watch_channel(channel_id);
                    }
                    NetworkServiceEvent::ChannelAbandon(channel_id) => {
                        self.store.remove_watch_channel(channel_id);
                    }
                    NetworkServiceEvent::RevokeAndAckReceived(
                        _peer_id,
                        channel_id,
                        revocation_data,
                        settlement_data,
                    ) => {
                        self.store
                            .update_revocation(channel_id, revocation_data, settlement_data);
                    }
                    NetworkServiceEvent::RemoteCommitmentSigned(
                        _peer_id,
                        channel_id,
                        _commitment_tx,
                        settlement_data,
                    ) => {
                        self.store
                            .update_local_settlement(channel_id, settlement_data);
                    }
                    _ => {
                        // ignore
                    }
                }
            }
            WatchtowerMessage::PeriodicCheck => self.periodic_check(state),
        }
        Ok(())
    }
}

impl<S> WatchtowerActor<S>
where
    S: InvoiceStore + WatchtowerStore,
{
    fn periodic_check(&self, state: &WatchtowerState) {
        let secret_key = state.secret_key;
        let rpc_url = state.config.rpc_url.clone();
        tokio::task::block_in_place(move || {
            let mut cell_collector = DefaultCellCollector::new(&rpc_url);

            for channel_data in self.store.get_watch_channels() {
                let ckb_client = CkbRpcClient::new(&rpc_url);
                let search_key = SearchKey {
                    script: channel_data.funding_tx_lock.clone().into(),
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

                                                        if blake160(
                                                            &channel_data
                                                                .remote_settlement_data
                                                                .x_only_aggregated_pubkey,
                                                        )
                                                        .0 == pub_key_hash
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
                                                                    let commitment_tx_out_point =
                                                                        OutPoint::new(
                                                                            tx.calc_tx_hash(),
                                                                            0,
                                                                        );
                                                                    match ckb_client.get_live_cell(
                                                                        commitment_tx_out_point
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
                                                                                    commitment_tx_out_point,
                                                                                    revocation_data,
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
                                                                        ckb_client,
                                                                        channel_data
                                                                            .remote_settlement_data
                                                                            .clone(),
                                                                        true,
                                                                        secret_key,
                                                                        &mut cell_collector,
                                                                        &self.store,
                                                                    );
                                                                }
                                                            }
                                                        } else {
                                                            try_settle_commitment_tx(
                                                                commitment_lock,
                                                                ckb_client,
                                                                channel_data
                                                                    .local_settlement_data
                                                                    .clone()
                                                                    .expect(
                                                                        "local settlement data",
                                                                    ),
                                                                false,
                                                                secret_key,
                                                                &mut cell_collector,
                                                                &self.store,
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
    secret_key: SecretKey,
    cell_collector: &mut DefaultCellCollector,
) -> Result<TransactionView, Box<dyn std::error::Error>> {
    let witness = [
        XUDT_COMPATIBLE_WITNESS.to_vec(),
        vec![0xFF],
        revocation_data.commitment_number.to_be_bytes().to_vec(),
        revocation_data.x_only_aggregated_pubkey.to_vec(),
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
        .cell_deps(get_cell_deps(
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
    let (cells, total_capacity) = cell_collector.collect_live_cells(&query, false)?;
    debug!(
        "cells len: {}, total_capacity: {}",
        cells.len(),
        total_capacity
    );

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
        debug!(
            "inputs_capacity: {}, change_output_occupied_capacity: {}, fee: {}",
            inputs_capacity, change_output_occupied_capacity, fee
        );
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

fn try_settle_commitment_tx<S: InvoiceStore>(
    commitment_lock: Script,
    ckb_client: CkbRpcClient,
    settlement_data: SettlementData,
    for_remote: bool,
    secret_key: SecretKey,
    cell_collector: &mut DefaultCellCollector,
    store: &S,
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

    find_preimages(search_key.clone(), &ckb_client, store);

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
                    let cell_output: CellOutput = cell.output.clone().into();
                    let commitment_tx_out_point =
                        OutPoint::new(cell.out_point.tx_hash.pack(), cell.out_point.index.value());
                    let lock_script_args = cell_output.lock().args().raw_data();
                    let since = u64::from_le_bytes(
                        lock_script_args[20..28].try_into().expect("u64 from slice"),
                    );
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
                        error!("Found an invalid since commitment cell: {:?}", cell);
                        continue;
                    }

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
                    let commitment_tx_hash = cell.out_point.tx_hash.clone();
                    if lock_script_args.len() > 36 {
                        info!(
                            "Found a force closed commitment tx with pending tlcs: {:#x}",
                            commitment_tx_hash
                        );
                        match ckb_client.get_transaction(commitment_tx_hash.clone()) {
                            Ok(Some(tx_with_status)) => {
                                if tx_with_status.tx_status.status != Status::Committed {
                                    error!("Cannot find the commitment tx: {:?}, status is {:?}, maybe ckb indexer bug?", tx_with_status.tx_status.status, commitment_tx_hash);
                                } else if let Some(tx) = tx_with_status.transaction {
                                    match tx.inner {
                                        Either::Left(tx) => {
                                            let tx: Transaction = tx.inner.into();
                                            let pending_tlcs =
                                                tx.witnesses().into_iter().find_map(|witness| {
                                                    let witness = witness.raw_data();
                                                    if witness.len() > 18
                                                        && witness[0..16] == XUDT_COMPATIBLE_WITNESS
                                                    {
                                                        let unlock_type = witness[16];
                                                        let pending_tlc_count = witness[17];
                                                        if unlock_type < 0xFE
                                                            && unlock_type < pending_tlc_count
                                                            && witness.len()
                                                                > 18 + 85
                                                                    * pending_tlc_count as usize
                                                        {
                                                            // use remaining tlcs as new pending tlcs
                                                            let remain = [
                                                                &witness[18..(18
                                                                    + 85 * unlock_type as usize)],
                                                                &witness[(18
                                                                    + 85 * (unlock_type + 1)
                                                                        as usize)
                                                                    ..(18
                                                                        + 85 * pending_tlc_count
                                                                            as usize)],
                                                            ]
                                                            .concat()
                                                            .to_vec();

                                                            Some(remain)
                                                        } else {
                                                            None
                                                        }
                                                    } else {
                                                        None
                                                    }
                                                });

                                            match build_settlement_tx_for_pending_tlcs(
                                                cell,
                                                cell_header,
                                                delay_epoch.unwrap(),
                                                settlement_data.clone(),
                                                for_remote,
                                                pending_tlcs,
                                                secret_key,
                                                cell_collector,
                                                current_time,
                                                current_epoch,
                                                store,
                                            ) {
                                                Ok(Some(tx)) => match ckb_client
                                                    .send_transaction(tx.data().into(), None)
                                                {
                                                    Ok(tx_hash) => {
                                                        info!("Settlement tx for pending tlcs: {:?} sent, tx_hash: {:#x}", tx, tx_hash);
                                                    }
                                                    Err(err) => {
                                                        error!("Failed to send settlement tx for pending tlcs: {:?}, error: {:?}", tx, err);
                                                    }
                                                },
                                                Ok(None) => {
                                                    info!("No need to settle the commitment tx: {:#x} with pending tlcs", commitment_tx_hash);
                                                }
                                                Err(err) => {
                                                    error!("Failed to build settlement tx for pending tlcs: {:?}", err);
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
                                    "Cannot find the commitment tx: {:?}, maybe ckb indexer bug?",
                                    commitment_tx_hash
                                );
                            }
                            Err(err) => {
                                error!("Failed to get commitment tx: {:?}", err);
                            }
                        }
                    } else {
                        info!(
                            "Found a force closed commitment tx without pending tlcs: {:#x}",
                            commitment_tx_hash
                        );
                        if cell_header.epoch().to_rational() + delay_epoch.unwrap().to_rational()
                            > current_epoch.to_rational()
                        {
                            debug!(
                                "Commitment tx: {:#x} is not ready to settle",
                                cell.out_point.tx_hash
                            );
                        } else {
                            match build_settlement_tx(
                                commitment_tx_out_point,
                                since,
                                settlement_data.clone(),
                                secret_key,
                                cell_collector,
                            ) {
                                Ok(tx) => match ckb_client.send_transaction(tx.data().into(), None)
                                {
                                    Ok(tx_hash) => {
                                        info!(
                                            "Settlement tx: {:?} sent, tx_hash: {:#x}",
                                            tx, tx_hash
                                        );
                                    }
                                    Err(err) => {
                                        error!(
                                            "Failed to send settlement tx: {:?}, error: {:?}",
                                            tx, err
                                        );
                                    }
                                },
                                Err(err) => {
                                    error!("Failed to build settlement tx: {:?}", err);
                                }
                            }
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
fn find_preimages<S: InvoiceStore>(search_key: SearchKey, ckb_client: &CkbRpcClient, store: &S) {
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
                                        for witness in tx.witnesses().into_iter() {
                                            let witness = witness.raw_data();
                                            if witness.len() > 18
                                                && witness[0..16] == XUDT_COMPATIBLE_WITNESS
                                            {
                                                let unlock_type = witness[16];
                                                let pending_tlc_count = witness[17];
                                                if unlock_type < 0xFE
                                                    && unlock_type < pending_tlc_count
                                                    && witness.len()
                                                        > 18 + 85 * pending_tlc_count as usize
                                                {
                                                    let tlc = Tlc(&witness[(18
                                                        + 85 * unlock_type as usize)
                                                        ..(18 + 85 * (unlock_type + 1) as usize)]);
                                                    let preimage: [u8; 32] = witness
                                                        [witness.len() - 32..]
                                                        .try_into()
                                                        .expect("checked length");
                                                    let payment_hash =
                                                        tlc.hash_algorithm().hash(preimage);
                                                    if payment_hash.starts_with(tlc.payment_hash())
                                                    {
                                                        info!("Found a preimage for payment hash: {:?}", payment_hash);
                                                        store.insert_payment_preimage(
                                                            payment_hash.into(),
                                                            preimage.into(),
                                                        ).expect("insert payment preimage should be ok");
                                                    } else {
                                                        warn!("Found a preimage for payment hash: {:?}, but not match the tlc", payment_hash);
                                                    }
                                                }
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

fn build_settlement_tx(
    commitment_tx_out_point: OutPoint,
    since: u64,
    settlement_data: SettlementData,
    secret_key: SecretKey,
    cell_collector: &mut DefaultCellCollector,
) -> Result<TransactionView, Box<dyn std::error::Error>> {
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

    let SettlementData {
        x_only_aggregated_pubkey,
        aggregated_signature,
        to_local_output,
        to_local_output_data,
        to_remote_output,
        to_remote_output_data,
        tlcs: _tlcs,
    } = settlement_data;

    let mut tx_builder = Transaction::default()
        .as_advanced_builder()
        .cell_deps(get_cell_deps(
            vec![Contract::CommitmentLock, Contract::Secp256k1Lock],
            &to_local_output.type_().to_opt(),
        )?)
        .input(
            CellInput::new_builder()
                .previous_output(commitment_tx_out_point)
                .since(since.pack())
                .build(),
        )
        .output(to_local_output.clone())
        .output_data(to_local_output_data)
        .output(to_remote_output.clone())
        .output_data(to_remote_output_data)
        .output(change_output.clone())
        .output_data(Bytes::default())
        .witness(
            create_witness_for_commitment_cell(x_only_aggregated_pubkey, aggregated_signature)
                .pack(),
        )
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
    let (cells, total_capacity) = cell_collector.collect_live_cells(&query, true)?;
    debug!(
        "cells len: {}, total_capacity: {}",
        cells.len(),
        total_capacity
    );

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
        debug!(
            "inputs_capacity: {}, change_output_occupied_capacity: {}, fee: {}",
            inputs_capacity, change_output_occupied_capacity, fee
        );
        if inputs_capacity >= change_output_occupied_capacity + fee {
            let new_change_output = change_output
                .as_builder()
                .capacity((inputs_capacity - fee).pack())
                .build();
            let outputs = vec![to_local_output, to_remote_output, new_change_output];
            let tx = tx_builder.set_outputs(outputs).build();
            let tx = sign_tx(tx, secret_key)?;
            return Ok(tx);
        }
    }

    Err(Box::new(RpcError::Other(anyhow!("Not enough capacity"))))
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
) -> Result<TransactionView, Box<dyn std::error::Error>> {
    let tx = tx.data();

    let message = tx.calc_tx_hash();
    let secp256k1_message = Message::from_digest_slice(&message.raw_data())?;
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
    let pending_tlc_count = settlement_witness[17] as usize;
    let signature_start = 18 + 85 * pending_tlc_count;
    settlement_witness.splice(signature_start..signature_start + 65, signature_bytes);

    let witness = tx.witnesses().get(1).expect("get witness at index 1");
    let mut blake2b = new_blake2b();
    blake2b.update(tx.calc_tx_hash().as_slice());
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

#[allow(clippy::too_many_arguments)]
fn build_settlement_tx_for_pending_tlcs<S: InvoiceStore>(
    commitment_tx_cell: Cell,
    cell_header: HeaderView,
    delay_epoch: EpochNumberWithFraction,
    settlement_data: SettlementData,
    for_remote: bool,
    pending_tlcs: Option<Vec<u8>>,
    secret_key: SecretKey,
    cell_collector: &mut DefaultCellCollector,
    current_time: u64,
    current_epoch: EpochNumberWithFraction,
    store: &S,
) -> Result<Option<TransactionView>, Box<dyn std::error::Error>> {
    let settlement_tlc: Option<(usize, SettlementTlc, Option<Hash256>)> = match pending_tlcs.clone()
    {
        Some(pending_tlcs) => pending_tlcs
            .chunks(85)
            .enumerate()
            .find_map(|(index, tlc)| {
                let tlc = Tlc(tlc);
                settlement_data.tlcs.iter().find_map(|settlement_tlc| {
                    let pubkey_hash = blake2b_256(settlement_tlc.local_key.pubkey().serialize());
                    if settlement_tlc.tlc_id.is_offered() {
                        if pubkey_hash.starts_with(tlc.local_pubkey_hash())
                            && settlement_tlc.expiry < current_time
                        {
                            Some((index, settlement_tlc.clone(), None))
                        } else if pubkey_hash.starts_with(tlc.remote_pubkey_hash()) {
                            store
                                .search_payment_preimage(tlc.payment_hash())
                                .map(|preimage| (index, settlement_tlc.clone(), Some(preimage)))
                        } else {
                            None
                        }
                    } else if pubkey_hash.starts_with(tlc.remote_pubkey_hash())
                        && settlement_tlc.expiry < current_time
                    {
                        Some((index, settlement_tlc.clone(), None))
                    } else if pubkey_hash.starts_with(tlc.local_pubkey_hash()) {
                        store
                            .search_payment_preimage(tlc.payment_hash())
                            .map(|preimage| (index, settlement_tlc.clone(), Some(preimage)))
                    } else {
                        None
                    }
                })
            }),
        None => settlement_data
            .tlcs
            .iter()
            .enumerate()
            .find_map(|(index, settlement_tlc)| {
                if settlement_tlc.tlc_id.is_offered() {
                    if settlement_tlc.expiry < current_time {
                        if for_remote {
                            Some((index, settlement_tlc.clone(), None))
                        } else {
                            None
                        }
                    } else if for_remote {
                        None
                    } else {
                        store
                            .get_invoice_preimage(&settlement_tlc.payment_hash)
                            .map(|preimage| (index, settlement_tlc.clone(), Some(preimage)))
                    }
                } else if settlement_tlc.expiry < current_time {
                    if for_remote {
                        None
                    } else {
                        Some((index, settlement_tlc.clone(), None))
                    }
                } else if for_remote {
                    store
                        .get_invoice_preimage(&settlement_tlc.payment_hash)
                        .map(|preimage| (index, settlement_tlc.clone(), Some(preimage)))
                } else {
                    None
                }
            }),
    };
    if let Some((index, tlc, preimage)) = settlement_tlc {
        let delay = if preimage.is_some() {
            // unlock with preimage should delay 1/3 of the epoch
            mul(delay_epoch, 1, 3)
        } else {
            // unlock with expiry should delay 2/3 of the epoch
            mul(delay_epoch, 2, 3)
        };
        if cell_header.epoch().to_rational() + delay.to_rational() > current_epoch.to_rational() {
            return Ok(None);
        }
        let pubkey = PublicKey::from_secret_key(&Secp256k1::new(), &secret_key);
        let args = blake160(pubkey.serialize().as_ref());
        let fee_provider_lock_script =
            get_script_by_contract(Contract::Secp256k1Lock, args.as_bytes());

        let change_output = CellOutput::new_builder()
            .lock(fee_provider_lock_script.clone())
            .build();
        let placeholder_witness_for_change = WitnessArgs::new_builder()
            .lock(Some(ckb_types::bytes::Bytes::from(vec![0u8; 65])).pack())
            .build();

        let old_pending_tlcs = pending_tlcs.unwrap_or_else(|| {
            settlement_data
                .tlcs
                .iter()
                .flat_map(|tlc| tlc.to_witness(for_remote))
                .collect()
        });
        let old_pending_tlcs_size = old_pending_tlcs.len() / 85;
        let new_pending_tlcs = if old_pending_tlcs_size > 1 {
            [
                &[old_pending_tlcs_size as u8 - 1],
                &old_pending_tlcs[..85 * index],
                &old_pending_tlcs[85 * (index + 1)..],
            ]
            .concat()
        } else {
            vec![]
        };
        let mut witness_for_commitment_cell = create_witness_for_commitment_cell_with_pending_tlcs(
            index as u8,
            old_pending_tlcs.as_slice(),
        );
        if let Some(preiamge) = preimage {
            witness_for_commitment_cell.extend_from_slice(preiamge.as_ref());
        }
        let cell_output: CellOutput = commitment_tx_cell.output.clone().into();
        let mut new_commitment_lock_script_args =
            cell_output.lock().args().raw_data()[0..36].to_vec();
        if !new_pending_tlcs.is_empty() {
            new_commitment_lock_script_args
                .extend_from_slice(&blake2b_256(&new_pending_tlcs)[0..20]);
        }
        if cell_output.type_().is_none() {
            let capacity: u64 = cell_output.capacity().unpack();
            let new_capacity = (capacity as u128 - tlc.payment_amount) as u64;
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
                .capacity((tlc.payment_amount as u64).pack())
                .build();

            let input = {
                if preimage.is_none() {
                    // TODO: ckb is using seconds as timestamp, we may need to change the expiry unit to seconds in the future
                    let since = Since::new(SinceType::Timestamp, tlc.expiry / 1000, false).value();
                    CellInput::new_builder()
                        .previous_output(commitment_tx_cell.out_point.clone().into())
                        .since(since.pack())
                        .build()
                } else {
                    CellInput::new_builder()
                        .previous_output(commitment_tx_cell.out_point.clone().into())
                        .build()
                }
            };
            let mut tx_builder = Transaction::default()
                .as_advanced_builder()
                .cell_deps(get_cell_deps(
                    vec![Contract::CommitmentLock, Contract::Secp256k1Lock],
                    &None,
                )?)
                .input(input)
                .output(new_commitment_output.clone())
                .output_data(Bytes::default())
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
            let (cells, total_capacity) = cell_collector.collect_live_cells(&query, false)?;
            debug!(
                "cells len: {}, total_capacity: {}",
                cells.len(),
                total_capacity
            );

            let mut inputs_capacity = capacity;
            for cell in cells {
                let input_capacity: u64 = cell.output.capacity().unpack();
                inputs_capacity += input_capacity;
                let since =
                    Since::new(SinceType::EpochNumberWithFraction, delay.full_value(), true)
                        .value();
                tx_builder = tx_builder.input(
                    CellInput::new_builder()
                        .previous_output(cell.out_point)
                        .since(since.pack())
                        .build(),
                );
                let fee = fee_calculator
                    .fee(tx_builder.clone().build().data().serialized_size_in_block() as u64);
                debug!(
                    "inputs_capacity: {}, new_capacity:  {}, settlement_output_occupied_capacity: {}, fee: {}",
                    inputs_capacity, new_capacity, settlement_output_occupied_capacity, fee
                );
                if inputs_capacity >= new_capacity + settlement_output_occupied_capacity + fee {
                    let adjusted_settlement_output = change_output
                        .as_builder()
                        .capacity((inputs_capacity - new_capacity - fee).pack())
                        .build();
                    let outputs = vec![new_commitment_output, adjusted_settlement_output];
                    let tx = tx_builder.set_outputs(outputs).build();
                    let tx = sign_tx_with_settlement(tx, secret_key, tlc.local_key.0)?;
                    return Ok(Some(tx));
                }
            }

            Err(Box::new(RpcError::Other(anyhow!("Not enough capacity"))))
        } else {
            let amount = u128::from_le_bytes(
                commitment_tx_cell.output_data.unwrap().as_bytes()[0..16]
                    .try_into()
                    .unwrap(),
            );
            let new_amount = amount - tlc.payment_amount;
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
                .build();
            let new_commitment_output_data = new_amount.to_le_bytes().to_vec().pack();
            let settlement_output = CellOutput::new_builder()
                .lock(fee_provider_lock_script.clone())
                .type_(cell_output.type_().clone())
                .build();
            let settlement_output_data = tlc.payment_amount.to_le_bytes().to_vec().pack();
            let settlement_output_occupied_capacity = settlement_output
                .occupied_capacity(
                    Capacity::bytes(settlement_output_data.raw_data().len()).unwrap(),
                )
                .expect("capacity does not overflow")
                .as_u64();
            let settlement_output = settlement_output
                .as_builder()
                .capacity(settlement_output_occupied_capacity.pack())
                .build();

            let input = {
                if preimage.is_none() {
                    // TODO: ckb is using seconds as timestamp, we may need to change the expiry unit to seconds in the future
                    let since = Since::new(SinceType::Timestamp, tlc.expiry / 1000, false).value();
                    CellInput::new_builder()
                        .previous_output(commitment_tx_cell.out_point.clone().into())
                        .since(since.pack())
                        .build()
                } else {
                    CellInput::new_builder()
                        .previous_output(commitment_tx_cell.out_point.clone().into())
                        .build()
                }
            };
            let mut tx_builder = Transaction::default()
                .as_advanced_builder()
                .cell_deps(get_cell_deps(
                    vec![Contract::CommitmentLock, Contract::Secp256k1Lock],
                    &commitment_tx_cell.output.type_.map(|script| script.into()),
                )?)
                .input(input)
                .output(new_commitment_output.clone())
                .output_data(new_commitment_output_data)
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
            let min_total_capacity =
                change_output_occupied_capacity + settlement_output_occupied_capacity + fee;
            let mut query = CellQueryOptions::new_lock(fee_provider_lock_script);
            query.script_search_mode = Some(SearchMode::Exact);
            query.secondary_script_len_range = Some(ValueRangeOption::new_exact(0));
            query.data_len_range = Some(ValueRangeOption::new_exact(0));
            query.min_total_capacity = min_total_capacity;
            let (cells, total_capacity) = cell_collector.collect_live_cells(&query, false)?;
            debug!(
                "cells len: {}, total_capacity: {}",
                cells.len(),
                total_capacity
            );
            let mut inputs_capacity = 0u64;
            for cell in cells {
                let input_capacity: u64 = cell.output.capacity().unpack();
                inputs_capacity += input_capacity;
                let since =
                    Since::new(SinceType::EpochNumberWithFraction, delay.full_value(), true)
                        .value();
                tx_builder = tx_builder.input(
                    CellInput::new_builder()
                        .previous_output(cell.out_point)
                        .since(since.pack())
                        .build(),
                );
                let fee = fee_calculator
                    .fee(tx_builder.clone().build().data().serialized_size_in_block() as u64);
                debug!("inputs_capacity: {}, change_output_occupied_capacity: {}, settlement_output_occupied_capacity: {}, fee: {}", inputs_capacity, change_output_occupied_capacity, settlement_output_occupied_capacity, fee);
                if inputs_capacity
                    >= change_output_occupied_capacity + settlement_output_occupied_capacity + fee
                {
                    let new_change_output = change_output
                        .as_builder()
                        .capacity(
                            (inputs_capacity - settlement_output_occupied_capacity - fee).pack(),
                        )
                        .build();
                    let outputs = vec![new_commitment_output, settlement_output, new_change_output];
                    let tx = tx_builder.set_outputs(outputs).build();
                    let tx = sign_tx_with_settlement(tx, secret_key, tlc.local_key.0)?;
                    return Ok(Some(tx));
                }
            }

            Err(Box::new(RpcError::Other(anyhow!("Not enough capacity"))))
        }
    } else if cell_header.epoch().to_rational() + delay_epoch.to_rational()
        > current_epoch.to_rational()
    {
        debug!(
            "Commitment tx: {:#x} is not ready to settle",
            commitment_tx_cell.out_point.tx_hash
        );
        Ok(None)
    } else {
        info!(
            "Try to settle the commitment tx: {:#x} discarding pending tlcs",
            commitment_tx_cell.out_point.tx_hash
        );
        let cell_output: CellOutput = commitment_tx_cell.output.into();
        let since = u64::from_le_bytes(
            cell_output.lock().args().raw_data()[20..28]
                .try_into()
                .expect("u64 from slice"),
        );
        let tx = build_settlement_tx(
            commitment_tx_cell.out_point.clone().into(),
            since,
            settlement_data,
            secret_key,
            cell_collector,
        )?;
        Ok(Some(tx))
    }
}

#[derive(Debug)]
struct Tlc<'a>(&'a [u8]);

impl<'a> Tlc<'a> {
    pub fn hash_algorithm(&self) -> HashAlgorithm {
        if (self.0[0] >> 1) & 0b0000001 == 0 {
            HashAlgorithm::CkbHash
        } else {
            HashAlgorithm::Sha256
        }
    }

    pub fn payment_hash(&self) -> &'a [u8] {
        &self.0[17..37]
    }

    pub fn remote_pubkey_hash(&self) -> &'a [u8] {
        &self.0[37..57]
    }

    pub fn local_pubkey_hash(&self) -> &'a [u8] {
        &self.0[57..77]
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

    // nomalize the fraction (max epoch length is 1800)
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
