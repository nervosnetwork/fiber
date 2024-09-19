use anyhow::anyhow;
use ckb_hash::new_blake2b;
use ckb_jsonrpc_types::{Either, Status};
use ckb_sdk::{
    rpc::ckb_indexer::{CellType, Order, ScriptType, SearchKey, SearchMode, Tx},
    traits::{CellCollector, CellQueryOptions, DefaultCellCollector, ValueRangeOption},
    transaction::builder::FeeCalculator,
    util::blake160,
    CkbRpcClient, RpcError,
};
use ckb_types::{
    self,
    core::{Capacity, TransactionView},
    packed::{Bytes, CellInput, CellOutput, OutPoint, Transaction, WitnessArgs},
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
    NetworkServiceEvent,
};

use super::{store::RevocationData, WatchtowerStore};

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
                    NetworkServiceEvent::ChannelReady(
                        peer_id,
                        channel_id,
                        funding_tx_out_point,
                    ) => {
                        let rpc_url = state.config.rpc_url.clone();
                        tokio::task::block_in_place(move || {
                            let ckb_client = CkbRpcClient::new(&rpc_url);
                            match ckb_client
                                .get_transaction(funding_tx_out_point.tx_hash().unpack())
                            {
                                Ok(Some(tx_with_status)) => {
                                    if tx_with_status.tx_status.status != Status::Committed {
                                        error!("Funding tx: {:?} is not committed yet, maybe it's a bug in the fn on_channel_ready", funding_tx_out_point);
                                    } else if let Some(tx) = tx_with_status.transaction {
                                        match tx.inner {
                                            Either::Left(tx) => {
                                                let tx: Transaction = tx.inner.into();
                                                self.store.insert_watch_channel(
                                                    channel_id,
                                                    tx.raw().outputs().get(0).unwrap().lock(),
                                                );
                                            }
                                            Either::Right(_tx) => {
                                                // unreachable, ignore
                                            }
                                        }
                                    }
                                }
                                Ok(None) => {
                                    error!("Cannot find funding tx: {:?} for channel: {:?} from peer: {:?}", funding_tx_out_point, channel_id, peer_id);
                                }
                                Err(err) => {
                                    error!("Failed to get funding tx: {:?}", err);
                                }
                            }
                        });
                    }
                    NetworkServiceEvent::ChannelClosed(_peer_id, channel_id, _close_tx_hash) => {
                        self.store.remove_watch_channel(channel_id);
                    }
                    NetworkServiceEvent::RevokeAndAckReceived(
                        _peer_id,
                        channel_id,
                        commitment_number,
                        aggregated_pubkey,
                        signature,
                        output,
                        output_data,
                    ) => {
                        self.store.update_revocation(
                            channel_id,
                            RevocationData {
                                commitment_number,
                                x_only_aggregated_pubkey: aggregated_pubkey,
                                aggregated_signature: signature,
                                output,
                                output_data,
                            },
                        );
                    }
                    _ => {
                        // ignore
                    }
                }
            }
            WatchtowerMessage::PeriodicCheck => {
                for channel_data in self.store.get_watch_channels() {
                    if channel_data.revocation_data.is_none() {
                        continue;
                    }
                    let revocation_data = channel_data.revocation_data.unwrap();
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
                        // we need two parties' signatures to unlock the funding tx, so we can check the last one transaction only to see if it's an old version commitment tx
                        match ckb_client.get_transactions(
                            search_key,
                            Order::Desc,
                            1u32.into(),
                            None,
                        ) {
                            Ok(txs) => {
                                if let Some(Tx::Ungrouped(tx)) = txs.objects.first() {
                                    if matches!(tx.io_type, CellType::Input) {
                                        match ckb_client.get_transaction(tx.tx_hash.clone()) {
                                            Ok(Some(tx_with_status)) => {
                                                if tx_with_status.tx_status.status
                                                    != Status::Committed
                                                {
                                                    error!("Cannot find the commitment tx: {:?}, status is {:?}, maybe ckb indexer bug?", tx_with_status.tx_status.status, tx.tx_hash);
                                                } else if let Some(tx) = tx_with_status.transaction
                                                {
                                                    match tx.inner {
                                                        Either::Left(tx) => {
                                                            let tx: Transaction = tx.inner.into();
                                                            if tx.raw().outputs().len() == 1 {
                                                                let output = tx
                                                                    .raw()
                                                                    .outputs()
                                                                    .get(0)
                                                                    .unwrap();
                                                                let lock_args =
                                                                    output.lock().args().raw_data();
                                                                let commitment_number =
                                                                    u64::from_le_bytes(
                                                                        lock_args[28..36]
                                                                            .try_into()
                                                                            .unwrap(),
                                                                    );
                                                                if revocation_data.commitment_number
                                                                    >= commitment_number
                                                                {
                                                                    warn!("Found an old version commitment tx: {:?}, revocation commitment number: {}, commitment number: {}", tx.calc_tx_hash(), revocation_data.commitment_number, commitment_number);
                                                                    let commitment_tx_out_point =
                                                                        OutPoint::new(
                                                                            tx.calc_tx_hash(),
                                                                            0,
                                                                        );
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
                                                    error!("Cannot find the commitment tx: {:?}, transcation is none, maybe ckb indexer bug?", tx.tx_hash);
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
                    });
                }
            }
        }
        Ok(())
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
        .unwrap()
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

            let tx = sign_revocation_tx(tx, secret_key)?;
            return Ok(tx);
        }
    }

    Err(Box::new(RpcError::Other(anyhow!("Not enough capacity"))))
}

fn sign_revocation_tx(
    tx: TransactionView,
    secret_key: SecretKey,
) -> Result<TransactionView, Box<dyn std::error::Error>> {
    let tx = tx.data();
    let witness = tx.witnesses().get(1).unwrap();
    let mut blake2b = new_blake2b();
    blake2b.update(tx.calc_tx_hash().as_slice());
    blake2b.update(&(witness.as_bytes().len() as u64).to_le_bytes());
    blake2b.update(&witness.as_bytes());
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
    let witnesses = vec![tx.witnesses().get(0).unwrap(), witness.as_bytes().pack()];

    Ok(tx.as_advanced_builder().set_witnesses(witnesses).build())
}
