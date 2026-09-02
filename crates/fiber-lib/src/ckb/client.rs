use crate::ckb::config::new_ckb_rpc_async_client;
#[cfg(target_arch = "wasm32")]
use crate::ckb::config::CKB_RPC_TIMEOUT;
use crate::ckb::CkbConfig;
use ckb_jsonrpc_types::{CellWithStatus, JsonBytes, OutPoint};
use ckb_sdk::rpc::ckb_indexer::{Cell, CellType, Order, Pagination, ScriptType, SearchKey, Tx};
use ckb_types::{prelude::Entity, prelude::IntoTransactionView, H256};

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
                transaction: response.transaction.and_then(|tx| match tx.inner {
                    ckb_jsonrpc_types::Either::Left(json) => Some(transaction_view_from_json(json)),
                    ckb_jsonrpc_types::Either::Right(bytes) => {
                        ckb_types::packed::Transaction::from_slice(bytes.as_bytes())
                            .ok()
                            .map(|packed| packed.into_view())
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
                transaction: response.transaction.and_then(|tx| match tx.inner {
                    ckb_jsonrpc_types::Either::Left(json) => Some(transaction_view_from_json(json)),
                    ckb_jsonrpc_types::Either::Right(bytes) => {
                        tracing::warn!(
                            "CKB RPC returned unexpected bytes transaction format ({} bytes), ignoring",
                            bytes.len()
                        );
                        None
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
    async fn get_live_cell(
        &self,
        out_point: OutPoint,
        with_data: bool,
    ) -> Result<CellWithStatus, anyhow::Error>;
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

/// On WASM, reqwest ClientBuilder lacks timeout, and
/// tokio::time::timeout panics (std::time::Instant not available).
/// Use gloo_timers future which works in both window and worker contexts.
/// Native keeps builder.timeout() for OS-level socket timeout.
#[cfg(target_arch = "wasm32")]
mod wasm_timeout {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    pub fn timeout<F>(dur: Duration, fut: F) -> WasmTimeout<F> {
        WasmTimeout {
            fut,
            timer: gloo_timers::future::TimeoutFuture::new(dur.as_millis() as u32),
        }
    }

    pub struct WasmTimeout<F> {
        fut: F,
        timer: gloo_timers::future::TimeoutFuture,
    }

    impl<F: Future> Future for WasmTimeout<F> {
        type Output = Result<F::Output, ()>;
        fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            let this = unsafe { self.get_unchecked_mut() };
            match unsafe { Pin::new_unchecked(&mut this.fut) }.poll(cx) {
                Poll::Ready(v) => Poll::Ready(Ok(v)),
                Poll::Pending => match Pin::new(&mut this.timer).poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Err(())),
                    Poll::Pending => Poll::Pending,
                },
            }
        }
    }

    // Safety: F is Send and TimeoutFuture is Send
    unsafe impl<F: Send> Send for WasmTimeout<F> {}
}

async fn with_ckb_rpc_timeout<F, T, E>(fut: F) -> Result<T, anyhow::Error>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: Into<anyhow::Error>,
{
    #[cfg(target_arch = "wasm32")]
    {
        wasm_timeout::timeout(CKB_RPC_TIMEOUT, fut)
            .await
            .map_err(|_| anyhow::anyhow!("CKB RPC timed out after {:?}", CKB_RPC_TIMEOUT))?
            .map_err(Into::into)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        fut.await.map_err(Into::into)
    }
}

#[async_trait::async_trait]
impl CkbChainClient for CkbRpcClient {
    async fn get_transaction(&self, hash: H256) -> Result<GetTxResponse, anyhow::Error> {
        let client = self.config.ckb_rpc_client();
        with_ckb_rpc_timeout(client.get_only_committed_packed_transaction(hash))
            .await
            .map(|resp| GetTxResponse::from(Some(resp)))
    }

    async fn get_live_cell(
        &self,
        out_point: OutPoint,
        with_data: bool,
    ) -> Result<CellWithStatus, anyhow::Error> {
        let client = self.config.ckb_rpc_client();
        client
            .get_live_cell(out_point, with_data)
            .await
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
        with_ckb_rpc_timeout(client.get_packed_header(block_hash.into()))
            .await
            .map(|maybe_bytes| {
                maybe_bytes.and_then(|bytes| {
                    match ckb_types::packed::Header::from_slice(bytes.as_bytes()) {
                        Ok(header) => Some(u64::from(header.raw().timestamp())),
                        Err(err) => {
                            tracing::warn!(
                                "failed to parse packed header ({} bytes): {:?}",
                                bytes.len(),
                                err
                            );
                            None
                        }
                    }
                })
            })
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
