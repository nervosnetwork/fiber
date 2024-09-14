use ckb_jsonrpc_types::{Either, Status};
use ckb_sdk::{
    rpc::ckb_indexer::{CellType, Order, ScriptType, SearchKey, SearchMode, Tx},
    CkbRpcClient,
};
use ckb_types::{packed::Transaction, prelude::Unpack};
use ractor::{Actor, ActorProcessingErr, ActorRef};
use secp256k1::SecretKey;
use tracing::{error, trace, warn};

use crate::{ckb::CkbConfig, NetworkServiceEvent};

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
                    NetworkServiceEvent::ChannelReady(peer_id, channel_id, funding_tx_hash) => {
                        let rpc_url = state.config.rpc_url.clone();
                        tokio::task::block_in_place(move || {
                            let ckb_client = CkbRpcClient::new(&rpc_url);
                            match ckb_client.get_transaction(funding_tx_hash.unpack()) {
                                Ok(Some(tx_with_status)) => {
                                    if tx_with_status.tx_status.status != Status::Committed {
                                        error!("Funding tx: {:?} is not committed yet, maybe it's a bug in the fn on_channel_ready", funding_tx_hash);
                                    } else {
                                        if let Some(tx) = tx_with_status.transaction {
                                            match tx.inner {
                                                Either::Left(tx) => {
                                                    let tx: Transaction = tx.inner.into();
                                                    self.store.insert_channel(
                                                        channel_id,
                                                        tx.raw().outputs().get(0).unwrap().lock(),
                                                    );
                                                }
                                                Either::Right(_tx) => {
                                                    // unreachable, ignore
                                                }
                                            }
                                        } else {
                                        }
                                    }
                                }
                                Ok(None) => {
                                    error!("Cannot find funding tx: {:?} for channel: {:?} from peer: {:?}", funding_tx_hash, channel_id, peer_id);
                                }
                                Err(err) => {
                                    error!("Failed to get funding tx: {:?}", err);
                                }
                            }
                        });
                    }
                    NetworkServiceEvent::ChannelClosed(_peer_id, channel_id, _close_tx_hash) => {
                        self.store.remove_channel(channel_id);
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
                for channel_data in self.store.get_channels() {
                    if channel_data.revocation_data.is_none() {
                        continue;
                    }
                    let revocation_data = channel_data.revocation_data.unwrap();
                    let rpc_url = state.config.rpc_url.clone();
                    tokio::task::block_in_place(move || {
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
                                                    error!("Cannot find the commitment tx: {:?}, maybe ckb indexer bug?", tx.tx_hash);
                                                } else {
                                                    if let Some(tx) = tx_with_status.transaction {
                                                        match tx.inner {
                                                            Either::Left(tx) => {
                                                                let tx: Transaction =
                                                                    tx.inner.into();
                                                                if tx.raw().outputs().len() == 1 {
                                                                    let output = tx
                                                                        .raw()
                                                                        .outputs()
                                                                        .get(0)
                                                                        .unwrap();
                                                                    let lock_args = output
                                                                        .lock()
                                                                        .args()
                                                                        .raw_data();
                                                                    let commitment_number =
                                                                        u64::from_le_bytes(
                                                                            lock_args[28..36]
                                                                                .try_into()
                                                                                .unwrap(),
                                                                        );
                                                                    if revocation_data
                                                                        .commitment_number
                                                                        >= commitment_number
                                                                    {
                                                                        warn!("Found an old version commitment tx: {:?}, revocation commitment number: {}, commitment number: {}", tx.calc_tx_hash(), revocation_data.commitment_number, commitment_number);
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
                                                    }
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
