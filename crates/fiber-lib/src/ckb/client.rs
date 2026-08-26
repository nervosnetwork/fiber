use crate::ckb::config::new_ckb_rpc_async_client;
#[cfg(target_arch = "wasm32")]
use crate::ckb::config::CKB_RPC_TIMEOUT;
use crate::ckb::CkbConfig;
use ckb_jsonrpc_types::JsonBytes;
use ckb_sdk::rpc::ckb_indexer::{Cell, CellType, Order, Pagination, ScriptType, SearchKey, Tx};
use ckb_types::{prelude::Entity, prelude::IntoTransactionView, H256};

use ckb_types::{
    core::{tx_pool::TxStatus, TransactionView},
    packed::{self, Script},
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

/// A committed transaction that spends an exact watched outpoint.
#[derive(Clone, Debug)]
pub struct CommittedOutPointSpend {
    /// The transaction spending the watched outpoint.
    pub transaction: TransactionView,
    /// The position of the watched outpoint in the transaction inputs.
    pub input_index: usize,
    /// The block number that committed the transaction.
    pub block_number: u64,
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

fn input_tx_hash(tx_item: &Tx) -> Option<H256> {
    match tx_item {
        Tx::Ungrouped(tx) if matches!(tx.io_type, CellType::Input) => Some(tx.tx_hash.clone()),
        Tx::Grouped(tx)
            if tx
                .cells
                .iter()
                .any(|(io_type, _)| matches!(io_type, CellType::Input)) =>
        {
            Some(tx.tx_hash.clone())
        }
        _ => None,
    }
}

fn first_input_tx_hash(txs: &[Tx]) -> Option<H256> {
    txs.iter().find_map(input_tx_hash)
}

#[allow(dead_code)]
pub(crate) fn find_watched_input_index(
    transaction: &TransactionView,
    watched_outpoint: &packed::OutPoint,
) -> Option<usize> {
    transaction
        .input_pts_iter()
        .position(|outpoint| outpoint == *watched_outpoint)
}

#[cfg(any(not(target_arch = "wasm32"), test))]
fn has_required_confirmations(tip: u64, block_number: u64, confirmations: u64) -> bool {
    tip.checked_sub(block_number)
        .and_then(|depth| depth.checked_add(1))
        .is_some_and(|depth| depth >= confirmations.max(1))
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

fn new_exact_lock_script_search_key(lock_script: &Script) -> SearchKey {
    SearchKey {
        script: lock_script.clone().into(),
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
    let search_key = new_exact_lock_script_search_key(funding_lock_script);

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
    let search_key = new_exact_lock_script_search_key(funding_lock_script);

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

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
pub(crate) async fn find_committed_outpoint_spend(
    rpc_url: &str,
    lock_script: &packed::Script,
    watched_outpoint: &packed::OutPoint,
    confirmations: u64,
) -> Result<Option<CommittedOutPointSpend>, anyhow::Error> {
    const PAGE_SIZE: u32 = 100;

    let client = new_ckb_rpc_async_client(rpc_url);
    let search_key = new_exact_lock_script_search_key(lock_script);
    let tip: u64 = client.get_tip_block_number().await?.into();
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
            .map_err(|error| anyhow::anyhow!("{error}"))?;

        if txs.objects.is_empty() {
            return Ok(None);
        }

        for tx_item in &txs.objects {
            let Some(tx_hash) = input_tx_hash(tx_item) else {
                continue;
            };
            let response = client
                .get_only_committed_packed_transaction(tx_hash)
                .await?;
            let response = GetTxResponse::from(Some(response));
            let TxStatus::Committed(block_number, _, _) = response.tx_status else {
                continue;
            };
            if !has_required_confirmations(tip, block_number, confirmations) {
                continue;
            }
            let Some(transaction) = response.transaction else {
                continue;
            };
            let Some(input_index) = find_watched_input_index(&transaction, watched_outpoint) else {
                continue;
            };

            return Ok(Some(CommittedOutPointSpend {
                transaction,
                input_index,
                block_number,
            }));
        }

        after_cursor = Some(txs.last_cursor);
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
    use super::{
        find_watched_input_index, first_input_tx_hash, has_required_confirmations, input_tx_hash,
    };
    use ckb_jsonrpc_types::{BlockNumber, Uint32};
    use ckb_sdk::rpc::ckb_indexer::{CellType, Tx, TxWithCell, TxWithCells};
    use ckb_types::{
        core::{TransactionBuilder, TransactionView},
        packed, H256,
    };

    fn build_tx(io_type: CellType, tx_hash: u8) -> Tx {
        Tx::Ungrouped(TxWithCell {
            tx_hash: H256::from([tx_hash; 32]),
            block_number: BlockNumber::from(1_u64),
            tx_index: Uint32::from(0_u32),
            io_index: Uint32::from(0_u32),
            io_type,
        })
    }

    fn build_outpoint(tx_hash: u8, index: u32) -> packed::OutPoint {
        packed::OutPoint::new(packed::Byte32::new([tx_hash; 32]), index)
    }

    fn build_transaction(inputs: Vec<packed::OutPoint>) -> TransactionView {
        TransactionBuilder::default()
            .inputs(
                inputs
                    .into_iter()
                    .map(|outpoint| packed::CellInput::new(outpoint, 0))
                    .collect::<Vec<_>>(),
            )
            .build()
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

    #[test]
    fn test_find_watched_input_index_returns_nonzero_exact_match() {
        let watched_outpoint = build_outpoint(2, 3);
        let transaction = build_transaction(vec![build_outpoint(1, 0), watched_outpoint.clone()]);

        assert_eq!(
            find_watched_input_index(&transaction, &watched_outpoint),
            Some(1)
        );
    }

    #[test]
    fn test_find_watched_input_index_rejects_different_output_index() {
        let transaction = build_transaction(vec![build_outpoint(2, 4)]);

        assert_eq!(
            find_watched_input_index(&transaction, &build_outpoint(2, 3)),
            None
        );
    }

    #[test]
    fn test_find_watched_input_index_rejects_unrelated_inputs() {
        let transaction = build_transaction(vec![build_outpoint(1, 0), build_outpoint(3, 3)]);

        assert_eq!(
            find_watched_input_index(&transaction, &build_outpoint(2, 3)),
            None
        );
    }

    #[test]
    fn test_grouped_transaction_with_input_is_candidate() {
        let tx_hash = H256::from([7; 32]);
        let grouped = Tx::Grouped(TxWithCells {
            tx_hash: tx_hash.clone(),
            block_number: BlockNumber::from(10_u64),
            tx_index: Uint32::from(0_u32),
            cells: vec![
                (CellType::Output, Uint32::from(0_u32)),
                (CellType::Input, Uint32::from(1_u32)),
            ],
        });

        assert_eq!(input_tx_hash(&grouped), Some(tx_hash));
    }

    #[test]
    fn test_required_confirmations_are_inclusive_and_zero_means_one() {
        assert!(has_required_confirmations(10, 10, 0));
        assert!(has_required_confirmations(10, 10, 1));
        assert!(has_required_confirmations(12, 10, 3));
        assert!(!has_required_confirmations(11, 10, 3));
        assert!(!has_required_confirmations(9, 10, 1));
    }
}
