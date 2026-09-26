//! In-process liquidity integration tests.
//!
//! These tests drive two real Fiber nodes over TCP against one shared, externally
//! controlled mock CKB chain. All cross-node lifecycle actions go through public
//! RPC. The mock chain is only touched to resolve pending transactions (commit /
//! reject) and observe VM effects; peer liquidity stores are never read or mutated.

use std::time::{Duration, Instant};

use ckb_types::{
    bytes::Bytes,
    core::TransactionView,
    packed::{CellInput, CellOutput, Script},
    prelude::{Builder, Entity, Pack},
};
use fiber_json_types::{
    channel::Channel, LiquidityAssetInfo, LiquidityAssetKind, LiquidityChainTransactionRole,
    LoopOutParams, ProviderAcceptLoopOutParams, ProviderQuoteLoopOutParams,
};

use crate::ckb::contracts::{get_cell_deps_by_contracts, get_script_by_contract, Contract};
use crate::fiber::Hash256;
use crate::liquidity::build_liquidity_lock_args;
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

async fn channel_snapshot(
    fixture: &LiquidityNetworkFixture,
    node: usize,
    channel_id: Hash256,
) -> Channel {
    let channel_id_json: fiber_json_types::Hash256 = channel_id.into();
    let channels = fixture.nodes[node].list_channels().await;
    channels
        .channels
        .into_iter()
        .find(|channel| channel.channel_id == channel_id_json)
        .unwrap_or_else(|| panic!("channel {channel_id_json} not found on node {node}"))
}

async fn channel_snapshot_before_deadline(
    fixture: &LiquidityNetworkFixture,
    node: usize,
    channel_id: Hash256,
    deadline: Instant,
    operation: &str,
) -> Result<Channel, String> {
    let channel_id_json: fiber_json_types::Hash256 = channel_id.into();
    fixture.nodes[node]
        .list_channels_before_deadline(deadline, operation)
        .await?
        .channels
        .into_iter()
        .find(|channel| channel.channel_id == channel_id_json)
        .ok_or_else(|| format!("{operation} returned no channel {channel_id_json}"))
}

fn channel_has_no_pending_tlcs(channel: &Channel) -> bool {
    channel.offered_tlc_balance == 0
        && channel.received_tlc_balance == 0
        && channel.pending_tlcs.is_empty()
}

async fn wait_for_channel_balances(
    fixture: &LiquidityNetworkFixture,
    channel_id: Hash256,
    expected_client: (u128, u128),
    expected_provider: (u128, u128),
    timeout: Duration,
) -> [Channel; 2] {
    let deadline = Instant::now() + timeout;
    let mut latest_client: Option<Channel> = None;
    let mut latest_provider: Option<Channel> = None;
    let expected = format!(
        "channel {channel_id:?} local/remote balances client {expected_client:?}, provider \
         {expected_provider:?}, with no pending TLCs"
    );
    loop {
        latest_client = Some(
            channel_snapshot_before_deadline(
                fixture,
                CLIENT,
                channel_id,
                deadline,
                &format!("list_channels request for client node {CLIENT}, expected {expected}"),
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{error}; latest client channel: {latest_client:?}; latest provider channel: \
                     {latest_provider:?}"
                )
            }),
        );
        latest_provider = Some(
            channel_snapshot_before_deadline(
                fixture,
                PROVIDER,
                channel_id,
                deadline,
                &format!("list_channels request for provider node {PROVIDER}, expected {expected}"),
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{error}; latest client channel: {latest_client:?}; latest provider channel: \
                     {latest_provider:?}"
                )
            }),
        );
        let client = latest_client.as_ref().expect("client channel fetched");
        let provider = latest_provider.as_ref().expect("provider channel fetched");
        if (client.local_balance, client.remote_balance) == expected_client
            && (provider.local_balance, provider.remote_balance) == expected_provider
            && channel_has_no_pending_tlcs(client)
            && channel_has_no_pending_tlcs(provider)
        {
            return [client.clone(), provider.clone()];
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected}; latest client local/remote {}/{}, provider \
                 local/remote {}/{}, client TLCs: {:?}, provider TLCs: {:?}",
                client.local_balance,
                client.remote_balance,
                provider.local_balance,
                provider.remote_balance,
                client.pending_tlcs,
                provider.pending_tlcs,
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_invalid_payout_and_assert_quiescent(
    fixture: &LiquidityNetworkFixture,
    channel_id: Hash256,
    swap_id: fiber_json_types::Hash256,
    expected_client: (u128, u128),
    expected_provider: (u128, u128),
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        let swap = fixture.nodes[CLIENT]
            .get_swap_before_deadline(
                deadline,
                "get_swap request while observing invalid payout",
                swap_id,
            )
            .await
            .unwrap_or_else(|error| panic!("{error}"))
            .expect("client swap must remain publicly visible");
        assert_eq!(
            swap.state, "payout_pending",
            "invalid committed payout must not advance the client swap"
        );

        let chain_transactions = fixture.nodes[CLIENT]
            .list_chain_transactions_before_deadline(
                deadline,
                "list chain transactions request while observing invalid payout",
                swap_id,
            )
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let definitive_failure = chain_transactions.transactions.iter().find(|transaction| {
            transaction.role == LiquidityChainTransactionRole::Payout
                && transaction.status == "confirmed"
                && transaction
                    .failure_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("payment_hash mismatch"))
        });
        if definitive_failure.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for confirmed payout validation failure; latest swap: \
                 {swap:?}; latest chain records: {chain_transactions:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let chain_transactions = fixture.nodes[CLIENT]
        .list_chain_transactions_before_deadline(
            deadline,
            "list chain transactions after invalid payout validation",
            swap_id,
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        chain_transactions
            .transactions
            .iter()
            .all(|transaction| transaction.role != LiquidityChainTransactionRole::Claim),
        "invalid payout must not produce a client claim: {chain_transactions:?}"
    );

    let payments = fixture.nodes[CLIENT]
        .list_payments_before_deadline(
            deadline,
            "list_payments request after invalid payout validation",
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        payments.payments.is_empty(),
        "fresh client must have no payment session after invalid payout: {payments:?}"
    );
    let client = channel_snapshot_before_deadline(
        fixture,
        CLIENT,
        channel_id,
        deadline,
        "list client channels after invalid payout validation",
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    let provider = channel_snapshot_before_deadline(
        fixture,
        PROVIDER,
        channel_id,
        deadline,
        "list provider channels after invalid payout validation",
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        (client.local_balance, client.remote_balance),
        expected_client
    );
    assert_eq!(
        (provider.local_balance, provider.remote_balance),
        expected_provider
    );
    assert!(
        channel_has_no_pending_tlcs(&client),
        "client must have no offered, received, or pending TLCs: {client:?}"
    );
    assert!(
        channel_has_no_pending_tlcs(&provider),
        "provider must have no offered, received, or pending TLCs: {provider:?}"
    );
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
    let expected_payment_principal = quote
        .amount
        .checked_add(quote.provider_fee)
        .expect("quoted payment principal must fit u128");

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

    let client_channel_before = channel_snapshot(&fixture, CLIENT, channel_id).await;
    let provider_channel_before = channel_snapshot(&fixture, PROVIDER, channel_id).await;

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
    let client_channel_pending = channel_snapshot(&fixture, CLIENT, channel_id).await;
    let provider_channel_pending = channel_snapshot(&fixture, PROVIDER, channel_id).await;
    assert_eq!(
        client_channel_pending.local_balance,
        client_channel_before.local_balance
    );
    assert_eq!(
        provider_channel_pending.local_balance,
        provider_channel_before.local_balance
    );
    for (side, channel) in [
        ("client", &client_channel_pending),
        ("provider", &provider_channel_pending),
    ] {
        assert_eq!(
            channel.offered_tlc_balance, 0,
            "{side} must not offer payment before payout confirmation"
        );
        assert_eq!(
            channel.received_tlc_balance, 0,
            "{side} must not receive payment before payout confirmation"
        );
        assert!(
            channel.pending_tlcs.is_empty(),
            "{side} must have no pending TLC before payout confirmation: {:?}",
            channel.pending_tlcs
        );
    }

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

    wait_for_channel_balances(
        &fixture,
        channel_id,
        (
            client_channel_before
                .local_balance
                .checked_sub(expected_payment_principal)
                .expect("client channel must fund payment principal"),
            client_channel_before
                .remote_balance
                .checked_add(expected_payment_principal)
                .expect("client remote balance must fit u128"),
        ),
        (
            provider_channel_before
                .local_balance
                .checked_add(expected_payment_principal)
                .expect("provider channel balance must fit u128"),
            provider_channel_before
                .remote_balance
                .checked_sub(expected_payment_principal)
                .expect("provider remote balance must fund payment principal"),
        ),
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
    let expected_payment_principal = quote
        .amount
        .checked_add(quote.provider_fee)
        .expect("quoted payment principal must fit u128");
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
    let client_channel_before = channel_snapshot(&fixture, CLIENT, channel_id).await;
    let provider_channel_before = channel_snapshot(&fixture, PROVIDER, channel_id).await;

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
    let settled_channels = wait_for_channel_balances(
        &fixture,
        channel_id,
        (
            client_channel_before
                .local_balance
                .checked_sub(expected_payment_principal)
                .expect("client channel must fund payment principal"),
            client_channel_before
                .remote_balance
                .checked_add(expected_payment_principal)
                .expect("client remote balance must fit u128"),
        ),
        (
            provider_channel_before
                .local_balance
                .checked_add(expected_payment_principal)
                .expect("provider channel balance must fit u128"),
            provider_channel_before
                .remote_balance
                .checked_sub(expected_payment_principal)
                .expect("provider remote balance must fund payment principal"),
        ),
        SWAP_TIMEOUT,
    )
    .await;
    let [settled_client, settled_provider] = settled_channels;
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
    wait_for_channel_balances(
        &fixture,
        channel_id,
        (settled_client.local_balance, settled_client.remote_balance),
        (
            settled_provider.local_balance,
            settled_provider.remote_balance,
        ),
        SWAP_TIMEOUT,
    )
    .await;
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

#[tokio::test]
async fn liquidity_ckb_loop_out_rejects_committed_payout_with_wrong_payment_hash() {
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

    let claimant_lock = Script::from_slice(
        &hex::decode(
            quote
                .claimant_lock
                .strip_prefix("0x")
                .expect("claimant lock hex prefix"),
        )
        .expect("decode claimant lock"),
    )
    .expect("parse claimant lock");
    let refund_lock = Script::from_slice(
        &hex::decode(
            quote
                .refund_lock
                .strip_prefix("0x")
                .expect("refund lock hex prefix"),
        )
        .expect("decode refund lock"),
    )
    .expect("parse refund lock");
    let payment_hash: Hash256 = quote.payment_hash.into();
    let mut wrong_payment_hash = [0u8; 32];
    wrong_payment_hash.copy_from_slice(payment_hash.as_ref());
    wrong_payment_hash[0] ^= 1;
    let lock_args = build_liquidity_lock_args(
        wrong_payment_hash,
        &claimant_lock,
        &refund_lock,
        quote.refund_after_lock_time,
        quote.amount,
        None,
    );
    let funding_output = CellOutput::new_builder()
        .capacity(quote.capacity_requirement_ckb)
        .lock(get_script_by_contract(
            Contract::Secp256k1Lock,
            b"invalid-payout-funding",
        ))
        .build();
    let funding_transaction = TransactionView::new_advanced_builder()
        .output(funding_output)
        .output_data(Bytes::new().pack())
        .build();
    let funding_outpoint = funding_transaction
        .output_pts_iter()
        .next()
        .expect("adversarial payout funding output");
    let funding_tx_hash = fixture
        .chain
        .submit_transaction(funding_transaction)
        .expect("submit adversarial payout funding transaction");
    fixture
        .commit(funding_tx_hash)
        .expect("commit adversarial payout funding transaction");

    let cell_deps =
        get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock, Contract::LiquidityLock])
            .await
            .expect("resolve adversarial payout cell deps");
    let malicious_payout = TransactionView::new_advanced_builder()
        .cell_deps(cell_deps)
        .input(
            CellInput::new_builder()
                .previous_output(funding_outpoint)
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(quote.capacity_requirement_ckb)
                .lock(get_script_by_contract(Contract::LiquidityLock, &lock_args))
                .build(),
        )
        .output_data(Bytes::new().pack())
        .build();
    let malicious_outpoint = malicious_payout
        .output_pts_iter()
        .next()
        .expect("malicious payout output");
    let malicious_tx_hash = fixture
        .chain
        .submit_transaction(malicious_payout)
        .expect("submit adversarial payout through chain controller");

    let client_channel_before = channel_snapshot(&fixture, CLIENT, channel_id).await;
    let provider_channel_before = channel_snapshot(&fixture, PROVIDER, channel_id).await;
    fixture.nodes[CLIENT]
        .loop_out(LoopOutParams {
            quote_id,
            max_provider_fee: MAX_PROVIDER_FEE,
            max_routing_fee: MAX_ROUTING_FEE,
            payout_outpoint: Some(malicious_outpoint.into()),
        })
        .await;
    fixture
        .wait_for_swap_state(CLIENT, quote_id, "payout_pending", SWAP_TIMEOUT)
        .await;

    fixture
        .commit(malicious_tx_hash)
        .expect("commit adversarial payout");
    fixture
        .wait_for_chain_tx(
            CLIENT,
            quote_id,
            LiquidityChainTransactionRole::Payout,
            "confirmed",
            SWAP_TIMEOUT,
        )
        .await;
    wait_for_invalid_payout_and_assert_quiescent(
        &fixture,
        channel_id,
        quote_id,
        (
            client_channel_before.local_balance,
            client_channel_before.remote_balance,
        ),
        (
            provider_channel_before.local_balance,
            provider_channel_before.remote_balance,
        ),
        SWAP_TIMEOUT,
    )
    .await;
    fixture.shutdown().await;
}
