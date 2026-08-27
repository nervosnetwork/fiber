//! In-process liquidity integration tests.
//!
//! These tests drive two real Fiber nodes over TCP against one shared, externally
//! controlled mock CKB chain. All cross-node lifecycle actions go through public
//! RPC. The mock chain is only touched to resolve pending transactions (commit /
//! reject) and observe VM effects; peer liquidity stores are never read or mutated.

use std::time::Duration;

use ckb_types::{
    bytes::Bytes,
    packed::Script,
    prelude::{Builder, Entity, Pack},
};
use fiber_json_types::{
    LiquidityAssetInfo, LiquidityAssetKind, LiquidityChainTransactionRole, LoopOutParams,
    ProviderAcceptLoopOutParams, ProviderQuoteLoopOutParams,
};

use crate::fiber::Hash256;
use crate::tests::liquidity_test_utils::LiquidityNetworkFixture;
use crate::tests::MIN_RESERVED_CKB;

const SWAP_TIMEOUT: Duration = Duration::from_secs(30);

const PROVIDER: usize = 0;
const CLIENT: usize = 1;

const LOOP_OUT_AMOUNT: u128 = 1_000;
const MAX_PROVIDER_FEE: u128 = 10;
const MAX_ROUTING_FEE: u128 = 100;

fn ckb_asset() -> LiquidityAssetInfo {
    LiquidityAssetInfo {
        asset_id: "ckb".to_string(),
        kind: LiquidityAssetKind::Ckb,
        udt_type_script: None,
        min_amount: 1,
        max_amount: 1_000_000,
        available_capacity: 1_000_000,
        base_fee: 1,
        proportional_fee_ppm: 0,
        enabled: true,
    }
}

fn claimant_lock_hex() -> String {
    let script = Script::new_builder()
        .args(Bytes::from_static(b"liquidity-e2e-claimant").pack())
        .build();
    format!("0x{}", hex::encode(script.as_slice()))
}

async fn client_local_balance(fixture: &LiquidityNetworkFixture, channel_id: Hash256) -> u128 {
    let channel_id_json: fiber_json_types::Hash256 = channel_id.into();
    let channels = fixture.nodes[CLIENT].list_channels().await;
    channels
        .channels
        .iter()
        .find(|channel| channel.channel_id == channel_id_json)
        .unwrap_or_else(|| panic!("client channel {channel_id_json} not found"))
        .local_balance
}

#[tokio::test]
async fn liquidity_ckb_loop_out_e2e() {
    let mut fixture = LiquidityNetworkFixture::new().await;
    let (channel_id, _) = fixture
        .establish_funded_channel(MIN_RESERVED_CKB, MIN_RESERVED_CKB + 200_000)
        .await
        .expect("establish funded channel");

    fixture.nodes[PROVIDER]
        .initialize_provider(ckb_asset())
        .await;

    let quote = fixture.nodes[PROVIDER]
        .provider_quote_loop_out(ProviderQuoteLoopOutParams {
            asset_id: "ckb".to_string(),
            amount: LOOP_OUT_AMOUNT,
            claimant_lock: claimant_lock_hex(),
            max_provider_fee: MAX_PROVIDER_FEE,
            max_routing_fee: MAX_ROUTING_FEE,
            expires_after_seconds: 60,
        })
        .await;
    let quote_id = quote.quote_id;

    fixture.nodes[CLIENT]
        .import_quote(quote.clone(), MAX_PROVIDER_FEE, MAX_ROUTING_FEE)
        .await;

    let provider_accept = fixture.nodes[PROVIDER]
        .provider_accept_loop_out(ProviderAcceptLoopOutParams { quote_id })
        .await;
    assert_eq!(provider_accept.swap_id, quote_id);
    let payout_outpoint = provider_accept
        .payout_outpoint
        .expect("provider accept must return the payout lock outpoint");

    let pending = fixture.pending_transactions();
    assert_eq!(
        pending.len(),
        1,
        "expected exactly one pending provider payout transaction"
    );
    let payout_tx_hash: Hash256 = Hash256::from(pending[0].hash());

    let balance_before = client_local_balance(&fixture, channel_id).await;

    let client_execute = fixture.nodes[CLIENT]
        .loop_out(LoopOutParams {
            quote_id,
            max_provider_fee: MAX_PROVIDER_FEE,
            max_routing_fee: MAX_ROUTING_FEE,
            payout_outpoint: Some(payout_outpoint),
        })
        .await;
    assert_eq!(client_execute.swap_id, quote_id);

    fixture
        .wait_for_swap_state(CLIENT, quote_id, "payout_pending", SWAP_TIMEOUT)
        .await;
    fixture
        .wait_for_swap_state(PROVIDER, quote_id, "payout_pending", SWAP_TIMEOUT)
        .await;

    let chain_txs = fixture.nodes[CLIENT]
        .list_chain_transactions(quote_id)
        .await;
    assert!(
        chain_txs
            .transactions
            .iter()
            .all(|transaction| transaction.role != LiquidityChainTransactionRole::Claim),
        "no claim record may exist before payout confirmation: {chain_txs:?}"
    );
    assert_eq!(
        client_local_balance(&fixture, channel_id).await,
        balance_before,
        "channel balance must not change before payout confirmation"
    );

    fixture.commit(payout_tx_hash).expect("commit payout");

    let claim_record = fixture
        .wait_for_chain_tx(
            CLIENT,
            quote_id,
            LiquidityChainTransactionRole::Claim,
            "broadcast",
            SWAP_TIMEOUT,
        )
        .await;

    fixture
        .commit(claim_record.tx_hash.into())
        .expect("commit claim");

    fixture
        .wait_for_swap_state(CLIENT, quote_id, "success", SWAP_TIMEOUT)
        .await;
    fixture
        .wait_for_swap_state(PROVIDER, quote_id, "success", SWAP_TIMEOUT)
        .await;

    let confirmed_claim = fixture
        .wait_for_chain_tx(
            CLIENT,
            quote_id,
            LiquidityChainTransactionRole::Claim,
            "confirmed",
            SWAP_TIMEOUT,
        )
        .await;
    assert_eq!(confirmed_claim.tx_hash, claim_record.tx_hash);

    fixture.shutdown().await;
}

#[tokio::test]
async fn liquidity_ckb_loop_out_provider_restart_discovers_committed_claim() {
    let mut fixture = LiquidityNetworkFixture::new().await;
    let (channel_id, _) = fixture
        .establish_funded_channel(MIN_RESERVED_CKB, MIN_RESERVED_CKB + 200_000)
        .await
        .expect("establish funded channel");

    fixture.nodes[PROVIDER]
        .initialize_provider(ckb_asset())
        .await;

    let quote = fixture.nodes[PROVIDER]
        .provider_quote_loop_out(ProviderQuoteLoopOutParams {
            asset_id: "ckb".to_string(),
            amount: LOOP_OUT_AMOUNT,
            claimant_lock: claimant_lock_hex(),
            max_provider_fee: MAX_PROVIDER_FEE,
            max_routing_fee: MAX_ROUTING_FEE,
            expires_after_seconds: 60,
        })
        .await;
    let quote_id = quote.quote_id;
    fixture.nodes[CLIENT]
        .import_quote(quote, MAX_PROVIDER_FEE, MAX_ROUTING_FEE)
        .await;

    let provider_accept = fixture.nodes[PROVIDER]
        .provider_accept_loop_out(ProviderAcceptLoopOutParams { quote_id })
        .await;
    let payout_outpoint = provider_accept
        .payout_outpoint
        .expect("provider accept must return the payout lock outpoint");
    let payout_transactions = fixture.pending_transactions();
    assert_eq!(payout_transactions.len(), 1);
    let payout_tx_hash = Hash256::from(payout_transactions[0].hash());

    fixture.nodes[CLIENT]
        .loop_out(LoopOutParams {
            quote_id,
            max_provider_fee: MAX_PROVIDER_FEE,
            max_routing_fee: MAX_ROUTING_FEE,
            payout_outpoint: Some(payout_outpoint),
        })
        .await;
    fixture.commit(payout_tx_hash).expect("commit payout");

    let claim_record = fixture
        .wait_for_chain_tx(
            CLIENT,
            quote_id,
            LiquidityChainTransactionRole::Claim,
            "broadcast",
            SWAP_TIMEOUT,
        )
        .await;
    fixture
        .wait_for_swap_state(PROVIDER, quote_id, "payment_settled", SWAP_TIMEOUT)
        .await;
    let settled_balance = client_local_balance(&fixture, channel_id).await;
    let pending_claims = fixture.pending_transactions();
    assert_eq!(
        pending_claims.len(),
        1,
        "only the client claim may be pending"
    );
    assert_eq!(
        Hash256::from(pending_claims[0].hash()),
        claim_record.tx_hash.into()
    );

    fixture.stop_node(PROVIDER).await;
    fixture
        .commit(claim_record.tx_hash.into())
        .expect("commit claim while provider is stopped");
    assert!(fixture.pending_transactions().is_empty());

    fixture
        .start_node_and_wait_ready(PROVIDER, channel_id, SWAP_TIMEOUT)
        .await;

    fixture
        .wait_for_swap_state(PROVIDER, quote_id, "success", SWAP_TIMEOUT)
        .await;
    fixture
        .wait_for_swap_state(CLIENT, quote_id, "success", SWAP_TIMEOUT)
        .await;
    assert_eq!(
        client_local_balance(&fixture, channel_id).await,
        settled_balance,
        "provider recovery must not dispatch a duplicate payment"
    );
    assert!(fixture.pending_transactions().is_empty());

    let client_chain_transactions = fixture.nodes[CLIENT]
        .list_chain_transactions(quote_id)
        .await;
    let claims = client_chain_transactions
        .transactions
        .iter()
        .filter(|transaction| transaction.role == LiquidityChainTransactionRole::Claim)
        .collect::<Vec<_>>();
    assert_eq!(
        claims.len(),
        1,
        "recovery must not create a duplicate claim"
    );
    assert_eq!(claims[0].tx_hash, claim_record.tx_hash);
    assert_eq!(claims[0].status, "confirmed");

    fixture.shutdown().await;
}
