use std::{collections::HashMap, sync::Arc};

use ckb_types::core::TransactionView;
use jsonrpsee::{
    core::async_trait,
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};

use ractor::{call_t, ActorRef};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tokio::sync::RwLock;

use crate::{
    ckb::CkbChainMessage,
    fiber::{network::DEFAULT_CHAIN_ACTOR_TIMEOUT, serde_utils::U64Hex, types::Hash256},
};

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct SubmitCommitmentTransactionParams {
    /// Channel ID
    channel_id: Hash256,
    /// Commitment number
    #[serde_as(as = "U64Hex")]
    commitment_number: u64,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]

pub(crate) struct SubmitCommitmentTransactionResult {
    /// Submitted commitment transaction hash
    tx_hash: Hash256,
}

/// RPC module for development purposes, this module is not intended to be used in production.
/// This module will be disabled in release build.
#[rpc(server)]
trait DevRpc {
    /// Submit a commitment transaction to the chain
    #[method(name = "submit_commitment_transaction")]
    async fn submit_commitment_transaction(
        &self,
        params: SubmitCommitmentTransactionParams,
    ) -> Result<SubmitCommitmentTransactionResult, ErrorObjectOwned>;
}

pub(crate) struct DevRpcServerImpl {
    ckb_chain_actor: ActorRef<CkbChainMessage>,
    commitment_txs: Arc<RwLock<HashMap<(Hash256, u64), TransactionView>>>,
}

impl DevRpcServerImpl {
    pub(crate) fn new(
        ckb_chain_actor: ActorRef<CkbChainMessage>,
        commitment_txs: Arc<RwLock<HashMap<(Hash256, u64), TransactionView>>>,
    ) -> Self {
        Self {
            ckb_chain_actor,
            commitment_txs,
        }
    }
}

#[async_trait]
impl DevRpcServer for DevRpcServerImpl {
    async fn submit_commitment_transaction(
        &self,
        params: SubmitCommitmentTransactionParams,
    ) -> Result<SubmitCommitmentTransactionResult, ErrorObjectOwned> {
        if let Some(tx) = self
            .commitment_txs
            .read()
            .await
            .get(&(params.channel_id, params.commitment_number))
        {
            if let Err(err) = call_t!(
                &self.ckb_chain_actor,
                CkbChainMessage::SendTx,
                DEFAULT_CHAIN_ACTOR_TIMEOUT,
                tx.clone()
            )
            .unwrap()
            {
                Err(ErrorObjectOwned::owned(
                    CALL_EXECUTION_FAILED_CODE,
                    err.to_string(),
                    Some(params),
                ))
            } else {
                Ok(SubmitCommitmentTransactionResult {
                    tx_hash: tx.hash().into(),
                })
            }
        } else {
            Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                "Commitment transaction not found".to_string(),
                Some(params),
            ))
        }
    }
}
