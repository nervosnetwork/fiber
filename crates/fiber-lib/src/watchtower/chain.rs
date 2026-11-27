use ckb_jsonrpc_types::{
    BlockNumber, CellWithStatus, HeaderView, JsonBytes, OutPoint, OutputsValidator, Timestamp,
    Transaction, TransactionWithStatusResponse, Uint32,
};
use ckb_sdk::{
    rpc::ckb_indexer::{Cell, Order, Pagination, SearchKey, Tx},
    CkbRpcClient, RpcError,
};
use ckb_types::{packed::Script, H256};

macro_rules! watchtower_chain {
    ($(fn $method:ident($($arg:ident: $arg_type:ty),*) -> $result:ty;)+) => {
        #[async_trait::async_trait]
        pub trait WatchtowerChain: Send + Sync {
            fn can_submit(&self) -> bool { true }
            fn as_rpc_client(&self) -> Option<&CkbRpcClient> { None }

            $(async fn $method(&self, $($arg: $arg_type),*) -> Result<$result, RpcError>;)+
        }

        #[async_trait::async_trait]
        impl WatchtowerChain for CkbRpcClient {
            fn as_rpc_client(&self) -> Option<&CkbRpcClient> {
                Some(self)
            }

            $(async fn $method(&self, $($arg: $arg_type),*) -> Result<$result, RpcError> {
                CkbRpcClient::$method(self, $($arg),*)
            })+
        }
    };
}

watchtower_chain! {
    fn get_transaction(hash: H256) -> Option<TransactionWithStatusResponse>;
    fn get_tip_header() -> HeaderView;
    fn get_header_by_number(number: BlockNumber) -> Option<HeaderView>;
    fn get_block_median_time(hash: H256) -> Option<Timestamp>;
    fn get_live_cell(outpoint: OutPoint, with_data: bool) -> CellWithStatus;
    fn get_cells(key: SearchKey, order: Order, limit: Uint32, after: Option<JsonBytes>) -> Pagination<Cell>;
    fn get_transactions(key: SearchKey, order: Order, limit: Uint32, after: Option<JsonBytes>) -> Pagination<Tx>;
    fn send_transaction(tx: Transaction, validator: Option<OutputsValidator>) -> H256;
}

/// Simulation backend injected into `WatchtowerActor` without coupling `fiber-lib` to a mock chain.
#[cfg(feature = "simulation")]
pub trait SimulatedWatchtowerBackend: WatchtowerChain {
    /// Local signer used by the watchtower when building fee-bearing transactions.
    fn signer(&self) -> crate::ckb::signer::LocalSigner;
    /// Cell collector for fee inputs in a single watchtower periodic scan.
    fn collector(&self) -> Box<dyn ckb_sdk::traits::CellCollector + Send>;
}

pub(super) async fn find_funding_spend(
    chain: &(impl WatchtowerChain + ?Sized),
    lock: &Script,
) -> Result<Option<H256>, RpcError> {
    use crate::ckb::client::{first_input_tx_hash, new_shutdown_tx_search_key};

    let key = new_shutdown_tx_search_key(lock);
    let mut after = None;
    loop {
        let page = chain
            .get_transactions(key.clone(), Order::Desc, 100u32.into(), after)
            .await?;
        if page.objects.is_empty() {
            return Ok(None);
        }
        if let Some(hash) = first_input_tx_hash(&page.objects) {
            return Ok(Some(hash));
        }
        after = Some(page.last_cursor);
    }
}
