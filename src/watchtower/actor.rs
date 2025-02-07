use anyhow::anyhow;
use ckb_hash::new_blake2b;
use ckb_jsonrpc_types::{Either, Status};
use ckb_sdk::{
    rpc::ckb_indexer::{CellType, Order, ScriptType, SearchKey, SearchMode, Tx, TxWithCell},
    traits::{CellCollector, CellQueryOptions, DefaultCellCollector, ValueRangeOption},
    transaction::builder::FeeCalculator,
    util::blake160,
    CkbRpcClient, RpcError,
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
use tracing::{error, info, trace, warn};

use crate::{
    ckb::{
        contracts::{get_cell_deps, get_script_by_contract, Contract},
        CkbConfig,
    },
    fiber::channel::{create_witness_for_commitment_cell, RevocationData, SettlementData},
    NetworkServiceEvent,
};

use super::{ChannelData, WatchtowerStore};

pub const DEFAULT_WATCHTOWER_CHECK_INTERVAL_SECONDS: u64 = 60;

pub struct WatchtowerActor<S> {
    store: S,
}

impl<S: WatchtowerStore> WatchtowerActor<S> {
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
    S: WatchtowerStore,
{
    fn periodic_check(&self, state: &WatchtowerState) {
        for channel_data in self.store.get_watch_channels() {
            let secret_key = state.secret_key;
            let rpc_url = state.config.rpc_url.clone();
            tokio::task::block_in_place(move || {
                let ckb_client = CkbRpcClient::new(&rpc_url);
                let mut cell_collector = DefaultCellCollector::new(&rpc_url);
                let search_key = SearchKey {
                    script: channel_data.funding_tx_lock.clone().into(),
                    script_type: ScriptType::Lock,
                    script_search_mode: Some(SearchMode::Exact),
                    with_data: None,
                    filter: None,
                    group_by_transaction: None,
                };

                if let Ok(txs) =
                    ckb_client.get_transactions(search_key, Order::Desc, 1u32.into(), None)
                {
                    if let Some(Tx::Ungrouped(tx)) = txs.objects.first() {
                        if matches!(tx.io_type, CellType::Input) {
                            handle_transaction(
                                &ckb_client,
                                &mut cell_collector,
                                &channel_data,
                                tx,
                                secret_key,
                            );
                        }
                    }
                } else {
                    error!("Failed to get transactions");
                }
            });
        }
    }
}

fn handle_transaction(
    ckb_client: &CkbRpcClient,
    cell_collector: &mut DefaultCellCollector,
    channel_data: &ChannelData,
    tx: &TxWithCell,
    secret_key: SecretKey,
) {
    match ckb_client.get_transaction(tx.tx_hash.clone()) {
        Ok(Some(tx_with_status)) => {
            if tx_with_status.tx_status.status != Status::Committed {
                error!(
                    "Cannot find the commitment tx: {:?}, status is {:?}, maybe ckb indexer bug?",
                    tx_with_status.tx_status.status, tx.tx_hash
                );
            } else if let Some(tx) = tx_with_status.transaction {
                if let Either::Left(tx) = tx.inner {
                    process_commitment_tx(
                        ckb_client,
                        cell_collector,
                        channel_data,
                        tx.inner.into(),
                        secret_key,
                    );
                } else {
                    // unreachable, ignore
                }
            } else {
                error!(
                "Cannot find the commitment tx: {:?}, transaction is none, maybe ckb indexer bug?",
                tx.tx_hash
            );
            }
        }
        Ok(None) => {
            error!(
                "Cannot find the commitment tx: {:?}, maybe ckb indexer bug?",
                tx.tx_hash
            );
        }
        Err(err) => {
            error!("Failed to get funding tx: {:?}", err);
        }
    }
}

fn process_commitment_tx(
    ckb_client: &CkbRpcClient,
    cell_collector: &mut DefaultCellCollector,
    channel_data: &ChannelData,
    tx: Transaction,
    secret_key: SecretKey,
) {
    if tx.raw().outputs().len() == 1 {
        let output = tx.raw().outputs().get(0).expect("get output 0 of tx");
        let commitment_lock = output.lock();
        let lock_args = commitment_lock.args().raw_data();
        let pub_key_hash: [u8; 20] = lock_args[0..20].try_into().expect("checked length");
        let commitment_number =
            u64::from_be_bytes(lock_args[28..36].try_into().expect("u64 from slice"));

        if blake160(&channel_data.remote_settlement_data.x_only_aggregated_pubkey).0 == pub_key_hash
        {
            handle_revocation(
                ckb_client,
                cell_collector,
                channel_data,
                tx,
                commitment_number,
                secret_key,
            );
        } else {
            try_settle_commitment_tx(
                ckb_client,
                commitment_lock,
                channel_data
                    .local_settlement_data
                    .clone()
                    .expect("remote settlement data"),
                secret_key,
                cell_collector,
            );
        }
    } else {
        // there may be a race condition that PeriodicCheck is triggered before the remove_channel fn is called
        // it's a close channel tx, ignore
    }
}

fn handle_revocation(
    ckb_client: &CkbRpcClient,
    cell_collector: &mut DefaultCellCollector,
    channel_data: &ChannelData,
    tx: Transaction,
    commitment_number: u64,
    secret_key: SecretKey,
) {
    match &channel_data.revocation_data {
        Some(revocation_data) if revocation_data.commitment_number >= commitment_number => {
            let commitment_tx_out_point = OutPoint::new(tx.calc_tx_hash(), 0);
            match ckb_client.get_live_cell(commitment_tx_out_point.clone().into(), false) {
                Ok(cell_with_status) => {
                    if cell_with_status.status == "live" {
                        warn!(
                            "Found an old version commitment tx submitted by remote: {:#x}",
                            tx.calc_tx_hash()
                        );
                        match build_revocation_tx(
                            commitment_tx_out_point,
                            revocation_data.clone(),
                            secret_key,
                            cell_collector,
                        ) {
                            Ok(tx) => match ckb_client.send_transaction(tx.data().into(), None) {
                                Ok(tx_hash) => {
                                    info!("Revocation tx: {:?} sent, tx_hash: {:?}", tx, tx_hash);
                                }
                                Err(err) => {
                                    error!("Failed to send revocation tx: {:?}", err);
                                }
                            },
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
        Some(_revocation_data) => {
            try_settle_commitment_tx(
                ckb_client,
                tx.raw()
                    .outputs()
                    .get(0)
                    .expect("get output 0 of tx")
                    .lock(),
                channel_data.remote_settlement_data.clone(),
                secret_key,
                cell_collector,
            );
        }
        None => {
            error!("No revocation data found");
        }
    }
}

fn build_revocation_tx(
    commitment_tx_out_point: OutPoint,
    revocation_data: RevocationData,
    secret_key: SecretKey,
    cell_collector: &mut DefaultCellCollector,
) -> Result<TransactionView, Box<dyn std::error::Error>> {
    let empty_witness_args = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];
    let witness = [
        empty_witness_args.to_vec(),
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
        ))
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

    let mut query = CellQueryOptions::new_lock(fee_provider_lock_script);
    query.script_search_mode = Some(SearchMode::Exact);
    query.secondary_script_len_range = Some(ValueRangeOption::new_exact(0));
    query.data_len_range = Some(ValueRangeOption::new_exact(0));
    let (cells, _total_capacity) = cell_collector.collect_live_cells(&query, true)?;

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

fn try_settle_commitment_tx(
    ckb_client: &CkbRpcClient,
    commitment_lock: Script,
    settlement_data: SettlementData,
    secret_key: SecretKey,
    cell_collector: &mut DefaultCellCollector,
) {
    let current_epoch = match ckb_client.get_tip_header() {
        Ok(tip_header) => {
            let tip_header: HeaderView = tip_header.into();
            tip_header.epoch()
        }
        Err(err) => {
            error!("Failed to get tip header: {:?}", err);
            return;
        }
    };

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
        group_by_transaction: None,
    };
    // the live cells number should be 1 or 0 for normal case, however, an attacker may create a lot of cells to implement a tx pinning attack.
    match ckb_client.get_cells(search_key, Order::Desc, 100u32.into(), None) {
        Ok(cells) => {
            for cell in cells.objects {
                let cell_output: CellOutput = cell.output.into();
                let commitment_tx_out_point =
                    OutPoint::new(cell.out_point.tx_hash.pack(), cell.out_point.index.value());
                let lock_script_args = cell_output.lock().args().raw_data();
                let since = u64::from_le_bytes(
                    lock_script_args[20..28].try_into().expect("u64 from slice"),
                );
                let header: HeaderView = match ckb_client.get_header_by_number(cell.block_number) {
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
                let since_epoch = EpochNumberWithFraction::from_full_value(since);
                if header.epoch().to_rational() + since_epoch.to_rational()
                    > current_epoch.to_rational()
                {
                    continue;
                }
                info!(
                    "Found a force closed commitment tx: {:#x}",
                    cell.out_point.tx_hash
                );
                match build_settlement_tx(
                    commitment_tx_out_point,
                    since,
                    settlement_data.clone(),
                    secret_key,
                    cell_collector,
                ) {
                    Ok(tx) => match ckb_client.send_transaction(tx.data().into(), None) {
                        Ok(tx_hash) => {
                            info!("Settlement tx: {:?} sent, tx_hash: {:#x}", tx, tx_hash);
                        }
                        Err(err) => {
                            error!("Failed to send settlement tx: {:?}, error: {:?}", tx, err);
                        }
                    },
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

fn build_settlement_tx(
    commitment_tx_out_point: OutPoint,
    since: u64,
    settlement_data: SettlementData,
    secret_key: SecretKey,
    cell_collector: &mut DefaultCellCollector,
) -> Result<TransactionView, Box<dyn std::error::Error>> {
    // TODO use 0x00 ~ 0xFD to get back the funds
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
    } = settlement_data;

    let mut tx_builder = Transaction::default()
        .as_advanced_builder()
        .cell_deps(get_cell_deps(
            vec![Contract::CommitmentLock, Contract::Secp256k1Lock],
            &to_local_output.type_().to_opt(),
        ))
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

    let mut query = CellQueryOptions::new_lock(fee_provider_lock_script);
    query.script_search_mode = Some(SearchMode::Exact);
    query.secondary_script_len_range = Some(ValueRangeOption::new_exact(0));
    query.data_len_range = Some(ValueRangeOption::new_exact(0));
    let (cells, _total_capacity) = cell_collector.collect_live_cells(&query, true)?;

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
