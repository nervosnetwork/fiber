use crate::ckb::config::new_ckb_rpc_async_client;
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

fn first_input_tx_hash(txs: &[Tx]) -> Option<H256> {
    txs.iter().find_map(|tx_item| match tx_item {
        Tx::Ungrouped(tx) if matches!(tx.io_type, CellType::Input) => Some(tx.tx_hash.clone()),
        _ => None,
    })
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

fn new_shutdown_tx_search_key(funding_lock_script: &Script) -> SearchKey {
    SearchKey {
        script: funding_lock_script.clone().into(),
        script_type: ScriptType::Lock,
        script_search_mode: Some(ckb_sdk::rpc::ckb_indexer::SearchMode::Exact),
        with_data: None,
        filter: None,
        group_by_transaction: None,
    }
}

/// Paginate the CKB indexer to find the first transaction whose `io_type` is
/// `CellType::Input` for the given funding lock script. Returns `None` if no
/// such transaction exists.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn find_first_input_tx_hash(
    client: &ckb_sdk::CkbRpcClient,
    funding_lock_script: &Script,
) -> Result<Option<H256>, anyhow::Error> {
    let search_key = new_shutdown_tx_search_key(funding_lock_script);

    const PAGE_SIZE: u32 = 100;
    let mut after_cursor: Option<JsonBytes> = None;
    loop {
        let txs = client
            .get_transactions(
                search_key.clone(),
                Order::Desc,
                PAGE_SIZE.into(),
                after_cursor,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if txs.objects.is_empty() {
            return Ok(None);
        }

        if let Some(tx_hash) = first_input_tx_hash(&txs.objects) {
            return Ok(Some(tx_hash));
        }

        after_cursor = Some(txs.last_cursor.clone());
    }
}

async fn find_first_input_tx_hash_async(
    client: &ckb_sdk::CkbRpcAsyncClient,
    funding_lock_script: &Script,
) -> Result<Option<H256>, anyhow::Error> {
    let search_key = new_shutdown_tx_search_key(funding_lock_script);

    const PAGE_SIZE: u32 = 100;
    let mut after_cursor: Option<JsonBytes> = None;
    loop {
        let txs = client
            .get_transactions(
                search_key.clone(),
                Order::Desc,
                PAGE_SIZE.into(),
                after_cursor,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if txs.objects.is_empty() {
            return Ok(None);
        }

        if let Some(tx_hash) = first_input_tx_hash(&txs.objects) {
            return Ok(Some(tx_hash));
        }

        after_cursor = Some(txs.last_cursor.clone());
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
        let indexer_client = new_ckb_rpc_async_client(&self.config.rpc_url);
        let Some(tx_hash) =
            find_first_input_tx_hash_async(&indexer_client, &funding_lock_script).await?
        else {
            return Ok(None);
        };

        let async_client = self.config.ckb_rpc_client();
        let tx_with_status = async_client.get_transaction(tx_hash).await?;
        Ok(Some(tx_with_status.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::first_input_tx_hash;
    use ckb_jsonrpc_types::{BlockNumber, Uint32};
    use ckb_sdk::rpc::ckb_indexer::{CellType, Tx, TxWithCell};
    use ckb_types::H256;

    fn build_tx(io_type: CellType, tx_hash: u8) -> Tx {
        Tx::Ungrouped(TxWithCell {
            tx_hash: H256::from([tx_hash; 32]),
            block_number: BlockNumber::from(1_u64),
            tx_index: Uint32::from(0_u32),
            io_index: Uint32::from(0_u32),
            io_type,
        })
    }

    #[test]
    fn test_find_first_input_tx_hash_returns_first_input_from_page() {
        let output_only_page = vec![build_tx(CellType::Output, 1), build_tx(CellType::Output, 2)];
        let input_page = vec![
            build_tx(CellType::Output, 3),
            build_tx(CellType::Input, 4),
            build_tx(CellType::Input, 5),
        ];

        assert_eq!(first_input_tx_hash(&output_only_page), None);
        assert_eq!(first_input_tx_hash(&input_page), Some(H256::from([4; 32])));
    }
}
