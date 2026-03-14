use crate::fiber::payment::SendPaymentCommand;
use crate::fiber::types::Hash256;
use crate::fiber::watchtower_query::{TlcWatchtowerStatus, WatchtowerQuerier};
use crate::gen_rand_sha256_hash;
use crate::invoice::{Currency, InvoiceBuilder};
use crate::test_utils::init_tracing;
use crate::tests::test_utils::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A mock watchtower querier that returns predefined TLC statuses.
pub struct MockWatchtowerQuerier {
    statuses: Mutex<HashMap<(Hash256, Hash256), TlcWatchtowerStatus>>,
}

impl MockWatchtowerQuerier {
    pub fn new() -> Self {
        Self {
            statuses: Mutex::new(HashMap::new()),
        }
    }

    /// Set the status that will be returned for a given (channel_id, payment_hash) query.
    pub fn set_status(
        &self,
        channel_id: Hash256,
        payment_hash: Hash256,
        status: TlcWatchtowerStatus,
    ) {
        self.statuses
            .lock()
            .unwrap()
            .insert((channel_id, payment_hash), status);
    }
}

impl std::fmt::Debug for MockWatchtowerQuerier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockWatchtowerQuerier").finish()
    }
}

#[async_trait::async_trait]
impl WatchtowerQuerier for MockWatchtowerQuerier {
    async fn query_tlc_status(
        &self,
        channel_id: &Hash256,
        payment_hash: &Hash256,
    ) -> Option<TlcWatchtowerStatus> {
        self.statuses
            .lock()
            .unwrap()
            .get(&(*channel_id, *payment_hash))
            .cloned()
    }
}

/// Test: a->b payment where the preimage is only available via watchtower querier.
///
/// Scenario:
///   1. node_b has a mock watchtower querier.
///   2. node_b creates an invoice WITHOUT storing the preimage in its normal store.
///   3. node_a sends payment to node_b using the invoice.
///   4. node_b's CheckChannels picks up the preimage from the watchtower querier
///      and settles the TLC with RemoveTlcFulfill.
///   5. node_a's payment should succeed.
#[tokio::test]
async fn test_payment_success_via_watchtower_preimage_direct() {
    init_tracing();

    let watchtower_querier = Arc::new(MockWatchtowerQuerier::new());

    // Create node_b with the mock watchtower querier
    let config_b = NetworkNodeConfigBuilder::new()
        .node_name(Some("node-b".to_string()))
        .base_dir_prefix("test-wt-direct-b-")
        .watchtower_querier(Some(watchtower_querier.clone()))
        .build();
    let mut node_b = NetworkNode::new_with_config(config_b).await;

    // Create node_a normally
    let config_a = NetworkNodeConfigBuilder::new()
        .node_name(Some("node-a".to_string()))
        .base_dir_prefix("test-wt-direct-a-")
        .build();
    let mut node_a = NetworkNode::new_with_config(config_a).await;

    // Connect and establish channel
    node_a.connect_to(&mut node_b).await;
    let (channel_id, _funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters::new(HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT),
    )
    .await;

    // Generate a preimage and build an invoice on node_b
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000000000))
        .payment_preimage(preimage)
        .payee_pub_key(node_b.pubkey.into())
        .build()
        .expect("build invoice");
    let payment_hash = *invoice.payment_hash();

    // Insert the invoice WITHOUT storing the preimage in the normal preimage store.
    // This simulates the scenario where the preimage is only known to the watchtower
    // (e.g., the node crashed and lost the preimage, but the watchtower observed it onchain).
    node_b.insert_invoice(invoice.clone(), None);

    // Configure the watchtower to return the preimage when queried for this channel + payment_hash.
    watchtower_querier.set_status(
        channel_id,
        payment_hash,
        TlcWatchtowerStatus {
            preimage: Some(preimage),
            is_settled: true,
        },
    );

    // Send payment from a to b using the invoice
    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment should succeed");

    // The CheckChannels timer on node_b (every 3s in debug mode) should pick up the
    // preimage from the watchtower, settle the received TLC, and propagate back to node_a.
    node_a.wait_until_success(res.payment_hash).await;
}

/// Test: a->b->c payment where c settles via watchtower (preimage available through
/// the watchtower on node_b), the TLC offered by a to b should be settled offchain,
/// and the payment should succeed.
///
/// Scenario:
///   1. node_b has a mock watchtower querier.
///   2. node_c creates an invoice WITHOUT storing the preimage.
///   3. node_a sends payment to node_c via node_b.
///   4. The TLC reaches node_c, but node_c cannot settle (no preimage in store).
///   5. node_b's watchtower querier returns the preimage (simulating watchtower
///      observing c's onchain settlement on the b-c channel).
///   6. node_b's CheckChannels picks up the preimage for the received TLC on the
///      a-b channel and sends RemoveTlcFulfill to node_a.
///   7. node_a's payment should succeed.
#[tokio::test]
async fn test_multi_hop_payment_success_via_watchtower_preimage() {
    init_tracing();

    let watchtower_querier = Arc::new(MockWatchtowerQuerier::new());

    // Create 3 nodes: a (normal), b (with watchtower querier), c (normal)
    let nodes = NetworkNode::new_n_interconnected_nodes_with_config(3, |i| {
        let mut builder = NetworkNodeConfigBuilder::new()
            .node_name(Some(format!("node-{}", i)))
            .base_dir_prefix(&format!("test-wt-multihop-{}-", i));
        if i == 1 {
            // node_b gets the watchtower querier
            builder = builder.watchtower_querier(Some(watchtower_querier.clone()));
        }
        builder.build()
    })
    .await;

    let [mut node_a, mut node_b, mut node_c]: [NetworkNode; 3] = nodes.try_into().expect("3 nodes");

    // Establish channels: a-b and b-c
    let (ab_channel_id, ab_funding_tx_hash) = establish_channel_between_nodes(
        &mut node_a,
        &mut node_b,
        ChannelParameters::new(HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT),
    )
    .await;

    // Submit funding tx to all nodes for graph discovery
    let ab_funding_tx = node_a
        .get_transaction_view_from_hash(ab_funding_tx_hash)
        .await
        .expect("get funding tx");
    node_c.submit_tx(ab_funding_tx.clone()).await;
    node_c.add_channel_tx(ab_channel_id, ab_funding_tx_hash);

    let (bc_channel_id, bc_funding_tx_hash) = establish_channel_between_nodes(
        &mut node_b,
        &mut node_c,
        ChannelParameters::new(HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT),
    )
    .await;

    // Submit funding tx to all nodes for graph discovery
    let bc_funding_tx = node_b
        .get_transaction_view_from_hash(bc_funding_tx_hash)
        .await
        .expect("get funding tx");
    node_a.submit_tx(bc_funding_tx.clone()).await;
    node_a.add_channel_tx(bc_channel_id, bc_funding_tx_hash);

    // Wait for graph to see both channels
    wait_for_network_graph_update(&node_a, 2).await;

    // Generate a preimage and build an invoice on node_c
    let preimage = gen_rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(1000000000))
        .payment_preimage(preimage)
        .payee_pub_key(node_c.pubkey.into())
        .build()
        .expect("build invoice");
    let payment_hash = *invoice.payment_hash();

    // Insert the invoice WITHOUT storing the preimage.
    // node_c has the invoice but cannot settle because the preimage is not in its store.
    node_c.insert_invoice(invoice.clone(), None);

    // Configure node_b's watchtower querier to return the preimage for the a-b channel.
    // This simulates: c settled the TLC onchain on the b-c channel with the preimage,
    // and b's watchtower observed it and can now provide the preimage.
    watchtower_querier.set_status(
        ab_channel_id,
        payment_hash,
        TlcWatchtowerStatus {
            preimage: Some(preimage),
            is_settled: true,
        },
    );

    // Send payment from a to c
    let res = node_a
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            ..Default::default()
        })
        .await
        .expect("send payment should succeed");

    // node_b's CheckChannels should find the preimage via watchtower for the received TLC
    // on the a-b channel and settle it offchain with RemoveTlcFulfill.
    // This propagates back to node_a, marking the payment as successful.
    node_a.wait_until_success(res.payment_hash).await;
}
