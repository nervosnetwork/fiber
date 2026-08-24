#![allow(dead_code)]

use std::time::{Duration, Instant};

use ckb_types::{core::TransactionView, packed::OutPoint};
use fiber_json_types::{
    AddLiquidityAssetParams, GetSwapParams, ImportLiquidityQuoteParams, LiquidityAssetInfo,
    LiquidityChainTransaction, LiquidityChainTransactionRole, LiquidityProviderStatus,
    LiquidityQuoteEnvelope, LiquiditySwapRecord, LiquiditySwapResponse,
    ListLiquidityChainTransactionsParams, ListLiquidityChainTransactionsResponse, LoopInParams,
    LoopOutParams, ProviderAcceptLoopInParams, ProviderAcceptLoopOutParams,
    ProviderQuoteLoopOutParams, QuoteLoopInParams, SetLiquidityProviderModeParams,
};
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use ractor::call;
use serde::{de::DeserializeOwned, Serialize};

use crate::ckb::tests::test_utils::MockChainController;
use crate::ckb::{CkbChainMessage, LiveCell};
use crate::fiber::Hash256;
use crate::tests::{
    establish_channel_between_nodes, gen_liquidity_rpc_config, ChannelParameters, NetworkNode,
    NetworkNodeConfigBuilder,
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// One real Fiber node with an HTTP client for its public liquidity RPC module.
pub(crate) struct LiquidityNetworkNode {
    node: NetworkNode,
    rpc: HttpClient,
}

impl LiquidityNetworkNode {
    fn new(node: NetworkNode) -> Self {
        let (_, address) = node.rpc_server.as_ref().expect("liquidity RPC server");
        let rpc = HttpClientBuilder::default()
            .build(format!("http://{address}"))
            .expect("build liquidity RPC client");
        Self { node, rpc }
    }

    async fn request<P, R>(&self, method: &str, params: P) -> R
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        self.rpc
            .request(method, rpc_params![params])
            .await
            .unwrap_or_else(|error| panic!("liquidity RPC {method} failed: {error}"))
    }

    pub(crate) fn pubkey(&self) -> crate::fiber::Pubkey {
        self.node.pubkey
    }

    pub(crate) async fn provider_status(&self) -> LiquidityProviderStatus {
        self.rpc
            .request("get_liquidity_provider_status", rpc_params![])
            .await
            .expect("get liquidity provider status")
    }

    pub(crate) async fn set_provider_mode(&self, enabled: bool) -> LiquidityProviderStatus {
        self.request(
            "set_liquidity_provider_mode",
            SetLiquidityProviderModeParams { enabled },
        )
        .await
    }

    pub(crate) async fn add_asset(&self, asset: LiquidityAssetInfo) -> LiquidityAssetInfo {
        self.request("add_liquidity_asset", AddLiquidityAssetParams { asset })
            .await
    }

    pub(crate) async fn initialize_provider(
        &self,
        asset: LiquidityAssetInfo,
    ) -> (LiquidityProviderStatus, LiquidityAssetInfo) {
        let status = self.set_provider_mode(true).await;
        let asset = self.add_asset(asset).await;
        (status, asset)
    }

    pub(crate) async fn provider_quote_loop_out(
        &self,
        params: ProviderQuoteLoopOutParams,
    ) -> LiquidityQuoteEnvelope {
        self.request("provider_quote_loop_out", params).await
    }

    pub(crate) async fn import_quote(
        &self,
        quote: LiquidityQuoteEnvelope,
        max_provider_fee: u128,
        max_routing_fee: u128,
    ) -> LiquidityQuoteEnvelope {
        self.request(
            "import_liquidity_quote",
            ImportLiquidityQuoteParams {
                quote,
                max_provider_fee,
                max_routing_fee,
            },
        )
        .await
    }

    pub(crate) async fn loop_out(&self, params: LoopOutParams) -> LiquiditySwapResponse {
        self.request("loop_out", params).await
    }

    pub(crate) async fn provider_accept_loop_out(
        &self,
        params: ProviderAcceptLoopOutParams,
    ) -> LiquiditySwapResponse {
        self.request("provider_accept_loop_out", params).await
    }

    pub(crate) async fn quote_loop_in(&self, params: QuoteLoopInParams) -> LiquidityQuoteEnvelope {
        self.request("quote_loop_in", params).await
    }

    pub(crate) async fn loop_in(&self, params: LoopInParams) -> LiquiditySwapResponse {
        self.request("loop_in", params).await
    }

    pub(crate) async fn provider_accept_loop_in(
        &self,
        params: ProviderAcceptLoopInParams,
    ) -> LiquiditySwapResponse {
        self.request("provider_accept_loop_in", params).await
    }

    async fn get_swap(&self, swap_id: fiber_json_types::Hash256) -> Option<LiquiditySwapRecord> {
        self.request("get_swap", GetSwapParams { swap_id }).await
    }

    async fn list_chain_transactions(
        &self,
        swap_id: fiber_json_types::Hash256,
    ) -> ListLiquidityChainTransactionsResponse {
        self.request(
            "list_liquidity_chain_transactions",
            ListLiquidityChainTransactionsParams { swap_id },
        )
        .await
    }

    pub(crate) async fn restart(&mut self) {
        self.node.restart().await;
        let (_, address) = self
            .node
            .rpc_server
            .as_ref()
            .expect("restarted liquidity RPC server");
        self.rpc = HttpClientBuilder::default()
            .build(format!("http://{address}"))
            .expect("rebuild liquidity RPC client");
    }

    async fn stop(&mut self) {
        self.node.stop().await;
    }
}

/// Two interconnected nodes sharing one externally controlled mock CKB chain.
pub(crate) struct LiquidityNetworkFixture {
    pub(crate) nodes: [LiquidityNetworkNode; 2],
    chain: MockChainController,
}

impl LiquidityNetworkFixture {
    pub(crate) async fn new() -> Self {
        let chain = MockChainController::new();
        let shared_chain_state = chain.shared_state();
        let nodes = NetworkNode::new_n_interconnected_nodes_with_config(2, move |index| {
            NetworkNodeConfigBuilder::new()
                .node_name(Some(format!("liquidity-node-{index}")))
                .base_dir_prefix(&format!("test-liquidity-node-{index}-"))
                .rpc_config(Some(gen_liquidity_rpc_config()))
                .mock_chain_state(shared_chain_state.clone())
                .build()
        })
        .await
        .into_iter()
        .map(LiquidityNetworkNode::new)
        .collect::<Vec<_>>()
        .try_into()
        .unwrap_or_else(|_| unreachable!("created exactly two liquidity nodes"));

        Self { nodes, chain }
    }

    pub(crate) fn pending_transactions(&self) -> Vec<TransactionView> {
        self.chain.pending_transactions()
    }

    pub(crate) fn commit(&self, tx_hash: Hash256) -> Result<(), String> {
        self.chain.commit(tx_hash)
    }

    pub(crate) fn reject(&self, tx_hash: Hash256, reason: impl Into<String>) -> Result<(), String> {
        self.chain.reject(tx_hash, reason)
    }

    pub(crate) async fn live_cell(&self, node: usize, outpoint: OutPoint) -> Option<LiveCell> {
        call!(
            self.nodes[node].node.chain_actor,
            CkbChainMessage::GetLiveCell,
            outpoint
        )
        .expect("chain actor alive")
        .expect("live-cell query succeeds")
    }

    pub(crate) async fn establish_funded_channel(
        &mut self,
        node_0_amount: u128,
        node_1_amount: u128,
    ) -> (Hash256, Hash256) {
        let chain = self.chain.clone();
        let (left, right) = self.nodes.split_at_mut(1);
        let node_0 = &mut left[0].node;
        let node_1 = &mut right[0].node;
        let establish = establish_channel_between_nodes(
            node_0,
            node_1,
            ChannelParameters::new(node_0_amount, node_1_amount),
        );
        let commit_funding = async move {
            let deadline = Instant::now() + FIXTURE_TIMEOUT;
            loop {
                if let Some(tx) = chain.pending_transactions().into_iter().next() {
                    let tx_hash = tx.hash().into();
                    chain.commit(tx_hash).expect("commit channel funding tx");
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for pending channel funding transaction"
                );
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        };

        let (channel, ()) = tokio::join!(establish, commit_funding);
        channel
    }

    pub(crate) async fn transfer_loop_out_quote(
        &self,
        provider: usize,
        client: usize,
        params: ProviderQuoteLoopOutParams,
        max_provider_fee: u128,
        max_routing_fee: u128,
    ) -> LiquidityQuoteEnvelope {
        let quote = self.nodes[provider].provider_quote_loop_out(params).await;
        self.nodes[client]
            .import_quote(quote, max_provider_fee, max_routing_fee)
            .await
    }

    pub(crate) async fn wait_for_swap_state(
        &self,
        node: usize,
        swap_id: fiber_json_types::Hash256,
        expected_state: &str,
        timeout: Duration,
    ) -> LiquiditySwapRecord {
        let deadline = Instant::now() + timeout;
        loop {
            let latest = self.nodes[node].get_swap(swap_id).await;
            if let Some(record) = latest.as_ref() {
                if record.state == expected_state {
                    return record.clone();
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for swap {swap_id:?} on node {node} to reach \
                     {expected_state}; latest swap record: {latest:?}"
                );
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    pub(crate) async fn wait_for_chain_tx(
        &self,
        node: usize,
        swap_id: fiber_json_types::Hash256,
        role: LiquidityChainTransactionRole,
        status: &str,
        timeout: Duration,
    ) -> LiquidityChainTransaction {
        let deadline = Instant::now() + timeout;
        loop {
            let latest = self.nodes[node]
                .list_chain_transactions(swap_id)
                .await
                .transactions;
            if let Some(transaction) = latest
                .iter()
                .find(|transaction| transaction.role == role && transaction.status == status)
            {
                return transaction.clone();
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for swap {swap_id:?} chain tx on node {node} with role \
                     {role:?} and status {status}; latest chain records: {latest:?}"
                );
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    pub(crate) async fn shutdown(mut self) {
        for node in &mut self.nodes {
            node.stop().await;
        }
    }
}

#[tokio::test]
async fn liquidity_network_fixture_provider_status_responds() {
    let mut fixture = LiquidityNetworkFixture::new().await;

    for node in &fixture.nodes {
        let status = node.provider_status().await;
        assert!(!status.enabled);
    }

    fixture.nodes[0].restart().await;
    assert!(!fixture.nodes[0].provider_status().await.enabled);

    fixture.shutdown().await;
}

#[tokio::test]
async fn liquidity_network_fixture_chain_control_is_narrow_and_operational() {
    use ckb_types::{
        bytes::Bytes,
        packed::CellOutput,
        prelude::{Builder, Entity, Pack},
    };

    let fixture = LiquidityNetworkFixture::new().await;
    let committed_output = CellOutput::new_builder().capacity(100u64).build();
    let committed_data = Bytes::from_static(b"liquidity fixture committed cell");
    let committed_tx = TransactionView::new_advanced_builder()
        .output(committed_output.clone())
        .output_data(committed_data.pack())
        .build();
    let committed_hash = committed_tx.hash().into();
    let committed_outpoint = committed_tx.output_pts_iter().next().expect("one output");
    let rejected_tx = TransactionView::new_advanced_builder().build();
    let rejected_hash = rejected_tx.hash().into();

    call!(
        fixture.nodes[0].node.chain_actor,
        CkbChainMessage::SendTx,
        committed_tx
    )
    .expect("chain actor alive")
    .expect("submit committed transaction");
    call!(
        fixture.nodes[0].node.chain_actor,
        CkbChainMessage::SendTx,
        rejected_tx
    )
    .expect("chain actor alive")
    .expect("submit rejected transaction");

    let pending_hashes = fixture
        .pending_transactions()
        .into_iter()
        .map(|tx| Hash256::from(tx.hash()))
        .collect::<Vec<_>>();
    assert!(pending_hashes.contains(&committed_hash));
    assert!(pending_hashes.contains(&rejected_hash));

    fixture.commit(committed_hash).expect("commit transaction");
    fixture
        .reject(rejected_hash, "fixture rejection")
        .expect("reject transaction");

    let live_cell = fixture
        .live_cell(1, committed_outpoint)
        .await
        .expect("committed output is live");
    assert_eq!(live_cell.output, committed_output);
    assert_eq!(live_cell.data.raw_data(), committed_data);

    fixture.shutdown().await;
}
