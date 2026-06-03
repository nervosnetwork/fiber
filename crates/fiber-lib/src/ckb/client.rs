use crate::ckb::CkbConfig;
use ckb_jsonrpc_types::JsonBytes;
use ckb_sdk::rpc::ckb_indexer::{Cell, CellType, Order, Pagination, ScriptType, SearchKey, Tx};
use ckb_types::H256;

use ckb_types::{
    core::{tx_pool::TxStatus, TransactionView},
    packed::Script,
};
use serde::{Deserialize, Serialize};

use crate::ckb::jsonrpc_types_convert::*;
use fiber_types::Hash256;

#[derive(Debug, Clone)]
pub struct GetTxResponse {
    /// The transaction.
    pub transaction: Option<TransactionView>,
    pub tx_status: TxStatus,
}

impl Default for GetTxResponse {
    fn default() -> Self {
        Self {
            transaction: None,
            tx_status: TxStatus::Unknown,
        }
    }
}

impl From<Option<ckb_jsonrpc_types::TransactionWithStatusResponse>> for GetTxResponse {
    fn from(value: Option<ckb_jsonrpc_types::TransactionWithStatusResponse>) -> Self {
        match value {
            Some(response) => Self {
                transaction: response.transaction.map(|tx| match tx.inner {
                    ckb_jsonrpc_types::Either::Left(json) => transaction_view_from_json(json),
                    ckb_jsonrpc_types::Either::Right(_) => {
                        panic!("bytes response format not used");
                    }
                }),
                tx_status: tx_status_from_json(response.tx_status),
            },
            None => Self::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GetShutdownTxResponse {
    /// The transaction.
    pub transaction: Option<TransactionView>,
    pub tx_status: TxStatus,
}

impl Default for GetShutdownTxResponse {
    fn default() -> Self {
        Self {
            transaction: None,
            tx_status: TxStatus::Unknown,
        }
    }
}

impl From<Option<ckb_jsonrpc_types::TransactionWithStatusResponse>> for GetShutdownTxResponse {
    fn from(value: Option<ckb_jsonrpc_types::TransactionWithStatusResponse>) -> Self {
        match value {
            Some(response) => Self {
                transaction: response.transaction.map(|tx| match tx.inner {
                    ckb_jsonrpc_types::Either::Left(json) => transaction_view_from_json(json),
                    ckb_jsonrpc_types::Either::Right(_) => {
                        panic!("bytes response format not used");
                    }
                }),
                tx_status: tx_status_from_json(response.tx_status),
            },
            None => Self::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GetCellsResponse {
    pub objects: Vec<Cell>,
    pub last_cursor: JsonBytes,
}

impl From<Pagination<Cell>> for GetCellsResponse {
    fn from(value: Pagination<Cell>) -> Self {
        Self {
            objects: value.objects,
            last_cursor: value.last_cursor,
        }
    }
}

#[async_trait::async_trait]
pub trait CkbChainClient: Send + Sync {
    async fn get_transaction(&self, hash: H256) -> Result<GetTxResponse, anyhow::Error>;
    async fn get_cells(
        &self,
        search_key: SearchKey,
        order: Order,
        limit: u32,
        after: Option<JsonBytes>,
    ) -> Result<Pagination<Cell>, anyhow::Error>;
    async fn get_block_timestamp(&self, block_hash: Hash256) -> Result<Option<u64>, anyhow::Error>;
    async fn get_shutdown_tx(
        &self,
        funding_lock_script: Script,
    ) -> Result<Option<GetShutdownTxResponse>, anyhow::Error>;
}

#[derive(Clone)]
pub struct CkbRpcClient {
    config: CkbConfig,
}

impl CkbRpcClient {
    pub fn new(config: &CkbConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }
}

#[async_trait::async_trait]
impl CkbChainClient for CkbRpcClient {
    async fn get_transaction(&self, hash: H256) -> Result<GetTxResponse, anyhow::Error> {
        let client = self.config.ckb_rpc_client();
        client
            .get_transaction(hash)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    async fn get_cells(
        &self,
        search_key: SearchKey,
        order: Order,
        limit: u32,
        after: Option<JsonBytes>,
    ) -> Result<Pagination<Cell>, anyhow::Error> {
        let client = self.config.ckb_rpc_client();
        client
            .get_cells(search_key, order, limit.into(), after)
            .await
            .map_err(Into::into)
    }

    async fn get_block_timestamp(&self, block_hash: Hash256) -> Result<Option<u64>, anyhow::Error> {
        let client = self.config.ckb_rpc_client();
        client
            .get_header(block_hash.into())
            .await
            .map(|x| x.map(|x| x.inner.timestamp.into()))
            .map_err(Into::into)
    }

    async fn get_shutdown_tx(
        &self,
        funding_lock_script: Script,
    ) -> Result<Option<GetShutdownTxResponse>, anyhow::Error> {
        let client = self.config.ckb_rpc_client();
        let search_key = SearchKey {
            script: funding_lock_script.into(),
            script_type: ScriptType::Lock,
            script_search_mode: Some(ckb_sdk::rpc::ckb_indexer::SearchMode::Exact),
            with_data: None,
            filter: None,
            group_by_transaction: None,
        };

        // Paginate to find the first Input (funding cell spend). A
        // single-page query with limit=1 can be blinded by a newer
        // output created with the same lock.
        const PAGE_SIZE: u32 = 200;
        let mut after_cursor: Option<JsonBytes> = None;
        let shutdown_tx_hash: Hash256 = 'search: loop {
            let txs = client
                .get_transactions(
                    search_key.clone(),
                    Order::Desc,
                    PAGE_SIZE.into(),
                    after_cursor,
                )
                .await?;

            if txs.objects.is_empty() {
                return Ok(None);
            }

            for tx_item in &txs.objects {
                if let Tx::Ungrouped(tx) = tx_item {
                    if matches!(tx.io_type, CellType::Input) {
                        break 'search tx.tx_hash.clone().into();
                    }
                }
            }

            after_cursor = Some(txs.last_cursor.clone());
        };

        let tx_with_status = client.get_transaction(shutdown_tx_hash.into()).await?;
        Ok(Some(tx_with_status.into()))
    }
}
