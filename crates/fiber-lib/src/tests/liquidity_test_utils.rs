use std::future::Future;
use std::time::{Duration, Instant};

use ckb_types::{core::TransactionView, packed::OutPoint};
use fiber_json_types::{
    AddLiquidityAssetParams, GetSwapParams, ImportLiquidityQuoteParams, LiquidityAssetInfo,
    LiquidityChainTransaction, LiquidityChainTransactionRole, LiquidityProviderStatus,
    LiquidityQuoteEnvelope, LiquiditySwapRecord, LiquiditySwapResponse,
    ListLiquidityChainTransactionsParams, ListLiquidityChainTransactionsResponse,
    ListPaymentsParams, ListPaymentsResult, LoopInParams, LoopOutParams,
    ProviderAcceptLoopInParams, ProviderAcceptLoopOutParams, ProviderQuoteLoopOutParams,
    QuoteLoopInParams, SetLiquidityProviderModeParams,
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
use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber::Hash256;
use crate::rpc::channel::{ChannelState, ListChannelsParams, ListChannelsResult};
use crate::rpc::peer::ListPeersResult;
use crate::tests::{
    establish_channel_between_nodes, gen_liquidity_rpc_config, ChannelParameters, NetworkNode,
    NetworkNodeConfigBuilder, MIN_RESERVED_CKB,
};
use crate::NetworkServiceEvent;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const CHANNEL_FUNDING_TIMEOUT: Duration = Duration::from_secs(5);

async fn await_before_deadline<T>(
    deadline: Instant,
    operation: &str,
    future: impl Future<Output = T>,
) -> Result<T, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(format!("{operation} exceeded its deadline"));
    }
    tokio::time::timeout(remaining, future)
        .await
        .map_err(|_| format!("{operation} exceeded its deadline"))
}

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

    async fn request_before_deadline<P, R>(
        &self,
        deadline: Instant,
        operation: &str,
        method: &str,
        params: P,
    ) -> Result<R, String>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        await_before_deadline(
            deadline,
            operation,
            self.rpc.request(method, rpc_params![params]),
        )
        .await?
        .map_err(|error| format!("{operation} failed: {error}"))
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

    #[allow(dead_code)]
    pub(crate) async fn set_provider_mode(&self, enabled: bool) -> LiquidityProviderStatus {
        self.request(
            "set_liquidity_provider_mode",
            SetLiquidityProviderModeParams { enabled },
        )
        .await
    }

    #[allow(dead_code)]
    pub(crate) async fn add_asset(&self, asset: LiquidityAssetInfo) -> LiquidityAssetInfo {
        self.request("add_liquidity_asset", AddLiquidityAssetParams { asset })
            .await
    }

    #[allow(dead_code)]
    pub(crate) async fn initialize_provider(
        &self,
        asset: LiquidityAssetInfo,
    ) -> (LiquidityProviderStatus, LiquidityAssetInfo) {
        let status = self.set_provider_mode(true).await;
        let asset = self.add_asset(asset).await;
        (status, asset)
    }

    #[allow(dead_code)]
    pub(crate) async fn provider_quote_loop_out(
        &self,
        params: ProviderQuoteLoopOutParams,
    ) -> LiquidityQuoteEnvelope {
        self.request("provider_quote_loop_out", params).await
    }

    #[allow(dead_code)]
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

    #[allow(dead_code)]
    pub(crate) async fn loop_out(&self, params: LoopOutParams) -> LiquiditySwapResponse {
        self.request("loop_out", params).await
    }

    #[allow(dead_code)]
    pub(crate) async fn provider_accept_loop_out(
        &self,
        params: ProviderAcceptLoopOutParams,
    ) -> LiquiditySwapResponse {
        self.request("provider_accept_loop_out", params).await
    }

    #[allow(dead_code)]
    pub(crate) async fn quote_loop_in(&self, params: QuoteLoopInParams) -> LiquidityQuoteEnvelope {
        self.request("quote_loop_in", params).await
    }

    #[allow(dead_code)]
    pub(crate) async fn loop_in(&self, params: LoopInParams) -> LiquiditySwapResponse {
        self.request("loop_in", params).await
    }

    #[allow(dead_code)]
    pub(crate) async fn provider_accept_loop_in(
        &self,
        params: ProviderAcceptLoopInParams,
    ) -> LiquiditySwapResponse {
        self.request("provider_accept_loop_in", params).await
    }

    #[allow(dead_code)]
    async fn get_swap(&self, swap_id: fiber_json_types::Hash256) -> Option<LiquiditySwapRecord> {
        self.request("get_swap", GetSwapParams { swap_id }).await
    }

    pub(crate) async fn get_swap_before_deadline(
        &self,
        deadline: Instant,
        operation: &str,
        swap_id: fiber_json_types::Hash256,
    ) -> Result<Option<LiquiditySwapRecord>, String> {
        self.request_before_deadline(deadline, operation, "get_swap", GetSwapParams { swap_id })
            .await
    }

    pub(crate) async fn list_chain_transactions(
        &self,
        swap_id: fiber_json_types::Hash256,
    ) -> ListLiquidityChainTransactionsResponse {
        self.request(
            "list_liquidity_chain_transactions",
            ListLiquidityChainTransactionsParams { swap_id },
        )
        .await
    }

    pub(crate) async fn list_chain_transactions_before_deadline(
        &self,
        deadline: Instant,
        operation: &str,
        swap_id: fiber_json_types::Hash256,
    ) -> Result<ListLiquidityChainTransactionsResponse, String> {
        self.request_before_deadline(
            deadline,
            operation,
            "list_liquidity_chain_transactions",
            ListLiquidityChainTransactionsParams { swap_id },
        )
        .await
    }

    pub(crate) async fn list_channels(&self) -> ListChannelsResult {
        self.request(
            "list_channels",
            ListChannelsParams {
                pubkey: None,
                include_closed: None,
                only_pending: None,
            },
        )
        .await
    }

    pub(crate) async fn list_channels_before_deadline(
        &self,
        deadline: Instant,
        operation: &str,
    ) -> Result<ListChannelsResult, String> {
        self.request_before_deadline(
            deadline,
            operation,
            "list_channels",
            ListChannelsParams {
                pubkey: None,
                include_closed: None,
                only_pending: None,
            },
        )
        .await
    }

    pub(crate) async fn list_payments_before_deadline(
        &self,
        deadline: Instant,
        operation: &str,
    ) -> Result<ListPaymentsResult, String> {
        self.request_before_deadline(
            deadline,
            operation,
            "list_payments",
            ListPaymentsParams {
                status: None,
                limit: Some(500),
                after: None,
            },
        )
        .await
    }

    async fn restart(&mut self) {
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
    pub(crate) chain: MockChainController,
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
    ) -> Result<(Hash256, Hash256), String> {
        let pending_before = self
            .chain
            .pending_transactions()
            .into_iter()
            .map(|tx| Hash256::from(tx.hash()))
            .collect::<Vec<_>>();
        if !pending_before.is_empty() {
            return Err(format!(
                "transactions were pending before channel funding: {pending_before:?}"
            ));
        }

        let chain = self.chain.clone();
        let channel_store = self.nodes[0].node.store.clone();
        let (left, right) = self.nodes.split_at_mut(1);
        let node_0 = &mut left[0].node;
        let node_1 = &mut right[0].node;
        let establish = establish_channel_between_nodes(
            node_0,
            node_1,
            ChannelParameters::new(node_0_amount, node_1_amount),
        );
        tokio::pin!(establish);
        let deadline = Instant::now() + CHANNEL_FUNDING_TIMEOUT;
        loop {
            tokio::select! {
                channel = &mut establish => return Ok(channel),
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }

            let pending = chain.pending_transactions();
            let pending_hashes = pending
                .iter()
                .map(|tx| Hash256::from(tx.hash()))
                .collect::<Vec<_>>();
            let funding_hashes = channel_store
                .get_all_channel_states()
                .into_iter()
                .filter_map(|state| {
                    state
                        .funding_tx
                        .as_ref()
                        .map(|tx| Hash256::from(tx.calc_tx_hash()))
                })
                .filter(|hash| pending_hashes.contains(hash))
                .collect::<Vec<_>>();
            match funding_hashes.as_slice() {
                [] if Instant::now() < deadline => continue,
                [] => {
                    return Err(format!(
                        "timed out waiting for a pending transaction matching persisted \
                             channel funding state; pending hashes: {pending_hashes:?}"
                    ));
                }
                [funding_hash] => {
                    chain.commit(*funding_hash)?;
                    break;
                }
                _ => {
                    return Err(format!(
                        "multiple pending transactions matched persisted channel funding state: \
                         {funding_hashes:?}; all pending hashes: {pending_hashes:?}"
                    ));
                }
            }
        }

        tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            establish,
        )
        .await
        .map_err(|_| "timed out waiting for channel ready after funding commit".to_string())
    }

    #[allow(dead_code)]
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

    pub(crate) async fn restart_node_and_wait_ready(
        &mut self,
        node: usize,
        channel_id: Hash256,
        timeout: Duration,
    ) {
        assert!(
            node < self.nodes.len(),
            "invalid liquidity node index {node}"
        );
        self.nodes[node].restart().await;
        self.wait_for_node_ready(node, channel_id, timeout).await;
    }

    pub(crate) async fn stop_node(&mut self, node: usize) {
        assert!(
            node < self.nodes.len(),
            "invalid liquidity node index {node}"
        );
        self.nodes[node].stop().await;
    }

    pub(crate) async fn start_node_and_wait_ready(
        &mut self,
        node: usize,
        channel_id: Hash256,
        timeout: Duration,
    ) {
        assert!(
            node < self.nodes.len(),
            "invalid liquidity node index {node}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        self.nodes[node].node.start().await;
        let (_, address) = self.nodes[node]
            .node
            .rpc_server
            .as_ref()
            .expect("restarted liquidity RPC server");
        self.nodes[node].rpc = HttpClientBuilder::default()
            .build(format!("http://{address}"))
            .expect("rebuild liquidity RPC client");
        self.wait_for_node_ready(node, channel_id, timeout).await;
    }

    async fn wait_for_node_ready(&mut self, node: usize, channel_id: Hash256, timeout: Duration) {
        let peer = 1 - node;
        let peer_pubkey: fiber_json_types::Pubkey = self.nodes[peer].pubkey().into();

        let deadline = Instant::now() + timeout;
        let mut saw_channel_ready_event = false;
        let mut latest_event = None;
        loop {
            while let Ok(event) = self.nodes[node].node.event_emitter.try_recv() {
                saw_channel_ready_event |= matches!(
                    event,
                    NetworkServiceEvent::ChannelReady(pubkey, ready_channel_id, _)
                        if pubkey == self.nodes[peer].pubkey() && ready_channel_id == channel_id
                );
                latest_event = Some(event);
            }
            let peers: ListPeersResult = await_before_deadline(
                deadline,
                "list_peers readiness request",
                self.nodes[node].request("list_peers", ()),
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{error} for liquidity node {node}, peer {peer_pubkey:?}, channel \
                     {channel_id:?}; saw ChannelReady event: {saw_channel_ready_event}; latest \
                     event: {latest_event:?}"
                )
            });
            let channels: ListChannelsResult = await_before_deadline(
                deadline,
                "list_channels readiness request",
                self.nodes[node].request(
                    "list_channels",
                    ListChannelsParams {
                        pubkey: None,
                        include_closed: None,
                        only_pending: None,
                    },
                ),
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{error} for liquidity node {node}, peer {peer_pubkey:?}, channel \
                     {channel_id:?}; latest peers: {peers:?}; saw ChannelReady event: \
                     {saw_channel_ready_event}; latest event: {latest_event:?}"
                )
            });
            let peer_ready = peers.peers.iter().any(|peer| peer.pubkey == peer_pubkey);
            let channel_ready = channels.channels.iter().any(|channel| {
                channel.channel_id == channel_id.into()
                    && channel.state == ChannelState::ChannelReady
            });
            if peer_ready && channel_ready && saw_channel_ready_event {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for liquidity node {node} restart readiness for peer \
                     {peer_pubkey:?} and channel {channel_id:?}; latest peers: {peers:?}; latest \
                     channels: {channels:?}; saw ChannelReady event: {saw_channel_ready_event}; \
                     latest event: {latest_event:?}"
                );
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    #[allow(dead_code)]
    pub(crate) async fn wait_for_swap_state(
        &self,
        node: usize,
        swap_id: fiber_json_types::Hash256,
        expected_state: &str,
        timeout: Duration,
    ) -> LiquiditySwapRecord {
        let deadline = Instant::now() + timeout;
        let mut latest = None;
        loop {
            let operation = format!(
                "get_swap request for node {node}, swap {swap_id:?}, expected state \
                 {expected_state}"
            );
            latest = self.nodes[node]
                .get_swap_before_deadline(deadline, &operation, swap_id)
                .await
                .unwrap_or_else(|error| panic!("{error}; latest swap record: {latest:?}"));
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

    #[allow(dead_code)]
    pub(crate) async fn wait_for_chain_tx(
        &self,
        node: usize,
        swap_id: fiber_json_types::Hash256,
        role: LiquidityChainTransactionRole,
        status: &str,
        timeout: Duration,
    ) -> LiquidityChainTransaction {
        let deadline = Instant::now() + timeout;
        let mut latest = Vec::new();
        loop {
            let operation = format!(
                "list chain transactions request for node {node}, swap {swap_id:?}, expected role \
                 {role:?} and status {status}"
            );
            latest = self.nodes[node]
                .list_chain_transactions_before_deadline(deadline, &operation, swap_id)
                .await
                .unwrap_or_else(|error| panic!("{error}; latest chain records: {latest:?}"))
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
    let fixture = LiquidityNetworkFixture::new().await;

    for node in &fixture.nodes {
        let status = node.provider_status().await;
        assert!(!status.enabled);
    }

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

    fixture
        .chain
        .submit_transaction(committed_tx)
        .expect("submit committed transaction");
    fixture
        .chain
        .submit_transaction(rejected_tx)
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

#[tokio::test]
async fn liquidity_network_fixture_restart_waits_for_peer_and_channel_readiness() {
    let mut fixture = LiquidityNetworkFixture::new().await;
    let (channel_id, _) = fixture
        .establish_funded_channel(MIN_RESERVED_CKB + 10_000, MIN_RESERVED_CKB)
        .await
        .expect("establish funded channel");

    fixture
        .restart_node_and_wait_ready(0, channel_id, Duration::from_secs(10))
        .await;

    fixture.shutdown().await;
}

#[tokio::test]
async fn liquidity_network_fixture_channel_setup_rejects_unrelated_pending_transaction() {
    let mut fixture = LiquidityNetworkFixture::new().await;
    let unrelated_tx = TransactionView::new_advanced_builder().build();
    let unrelated_hash = Hash256::from(unrelated_tx.hash());
    fixture
        .chain
        .submit_transaction(unrelated_tx)
        .expect("submit unrelated transaction");

    let error = fixture
        .establish_funded_channel(MIN_RESERVED_CKB + 10_000, MIN_RESERVED_CKB)
        .await
        .expect_err("unrelated pending transaction must reject channel setup");

    assert!(error.contains("pending before channel funding"));
    assert!(error.contains(&format!("{unrelated_hash:?}")));
    assert!(fixture
        .pending_transactions()
        .iter()
        .any(|tx| Hash256::from(tx.hash()) == unrelated_hash));

    fixture.shutdown().await;
}

#[tokio::test]
async fn liquidity_network_fixture_channel_setup_ignores_concurrent_unrelated_transaction() {
    let mut fixture = LiquidityNetworkFixture::new().await;
    let chain = fixture.chain.clone();
    let unrelated_tx = TransactionView::new_advanced_builder().build();
    let unrelated_hash = Hash256::from(unrelated_tx.hash());

    let (channel, ()) = tokio::join!(
        fixture.establish_funded_channel(MIN_RESERVED_CKB + 10_000, MIN_RESERVED_CKB),
        async move {
            tokio::task::yield_now().await;
            chain
                .submit_transaction(unrelated_tx)
                .expect("submit concurrent unrelated transaction");
        }
    );
    channel.expect("establish channel despite unrelated pending transaction");

    assert!(fixture
        .pending_transactions()
        .iter()
        .any(|tx| Hash256::from(tx.hash()) == unrelated_hash));

    fixture.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn liquidity_network_fixture_request_deadline_bounds_stalled_rpc() {
    let deadline = Instant::now() + Duration::from_millis(50);

    let error = await_before_deadline(
        deadline,
        "list_peers readiness request",
        std::future::pending::<()>(),
    )
    .await
    .expect_err("stalled RPC must respect fixture deadline");

    assert!(error.contains("list_peers readiness request"));
    assert!(error.contains("deadline"));
}
