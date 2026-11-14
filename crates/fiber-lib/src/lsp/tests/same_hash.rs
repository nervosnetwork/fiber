use super::*;
use crate::fiber_types::{PaymentSessionKey, TlcInfo, UpstreamTlcKey};

trait HostedTlcLookup {
    fn find_hosted_tlc(
        &self,
        hash: Hash256,
        previous: &crate::fiber_types::PrevTlcInfo,
    ) -> Option<&TlcInfo>;
}
impl HostedTlcLookup for crate::fiber::channel::ChannelActorState {
    fn find_hosted_tlc(
        &self,
        hash: Hash256,
        previous: &crate::fiber_types::PrevTlcInfo,
    ) -> Option<&TlcInfo> {
        self.tlc_state.offered_tlcs.tlcs.iter().find(|tlc| {
            tlc.payment_hash == hash
                && tlc.upstream_tlc_key
                    == Some(UpstreamTlcKey {
                        incoming_channel_id: previous.prev_channel_id,
                        incoming_tlc_id: previous.prev_tlc_id,
                    })
        })
    }
}
use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber_types::{Hash256, RemoveTlcFulfill, TlcErr, TlcErrPacket};
use crate::lsp::LspPaymentDeliveryStatus as DeliveryStatus;

async fn settle_desk_hold(desk: &NetworkNode, hash: Hash256, preimage: Hash256) {
    let result = desk.settle_invoice(&hash, preimage).await;
    // Learning the preimage from the outgoing payment may already settle the hold
    // via the normal maintenance loop. Do not race that legitimate automatic path.
    assert!(
        result.is_ok() || desk.get_invoice_status(&hash) == Some(CkbInvoiceStatus::Paid),
        "{result:?}"
    );
}

async fn wait_for_buyer_delivery(
    network: &LspTestNetwork,
    hash: Hash256,
) -> crate::lsp::LspPaymentDelivery {
    wait_until_async_timeout(|| async {
        hosted_deliveries(&network.nodes[0], hash)
            .first()
            .is_some_and(|d| d.status == DeliveryStatus::InFlight)
    })
    .await;
    hosted_deliveries(&network.nodes[0], hash).remove(0)
}

async fn redispatch(network: &LspTestNetwork, delivery: &crate::lsp::LspPaymentDelivery) {
    ractor::call_t!(
        network.nodes[0].network_actor,
        |reply| NetworkActorMessage::new_command(FiberActorCommand::DispatchBufferedTrampoline {
            request: delivery.request.clone(),
            reply,
        }),
        5_000
    )
    .unwrap()
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_session_retries_before_reporting_no_remaining_route() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let buyer = network.tenant(0, "buyer");
    let preimage = gen_rand_sha256_hash();
    let invoice = new_desk_trade_invoice(buyer, 1_000_000, preimage, true).await;
    let hash = *invoice.payment_hash();
    desk_pays_the_buyer(&network, &invoice).await;
    wait_until_async_timeout(|| async {
        buyer.node.get_invoice_status(&hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    let delivery = wait_for_buyer_delivery(&network, hash).await;
    let key = delivery.request.hosted_session_key();
    let original = network.nodes[0].store.get_payment_session(key).unwrap();
    let received = buyer
        .node
        .get_channel_actor_state(buyer.private_channel_id)
        .tlc_state
        .received_tlcs
        .tlcs
        .iter()
        .find(|tlc| tlc.payment_hash == hash)
        .unwrap()
        .clone();
    // A transient failure permits a PaymentActor retry. This fixture has only
    // one recipient/path: after that node is penalized, the retry's route search
    // must terminate cleanly rather than report the initial failure immediately.
    let channel = buyer
        .node
        .get_channel_actor(buyer.private_channel_id)
        .await
        .unwrap();
    ractor::call_t!(
        channel,
        |reply| crate::fiber::channel::ChannelActorMessage::Command(
            crate::fiber::channel::ChannelCommand::RemoveTlc(
                crate::fiber::channel::RemoveTlcCommand {
                    id: u64::from(received.tlc_id),
                    reason: RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                        TlcErr::new(TlcErrorCode::TemporaryNodeFailure),
                        &received.shared_secret,
                    )),
                },
                reply
            )
        ),
        5_000
    )
    .unwrap()
    .unwrap();
    network.nodes[1].wait_until_failed(hash).await;
    let finished = network.nodes[0].store.get_payment_session(key).unwrap();
    assert_eq!(finished.created_at, original.created_at);
    assert_eq!(finished.status, PaymentStatus::Failed);
    assert!(finished.attempts().any(|attempt| attempt.tried_times > 1));
    assert!(finished
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("path") || error.contains("route")));
    assert_eq!(
        hosted_deliveries(&network.nodes[0], hash)[0].attempt_count,
        1
    );
    assert!(network.nodes[0].store.get_payment_session(hash).is_none());
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_public_session_does_not_block_same_hash_hosted_payment() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let preimage = gen_rand_sha256_hash();
    let invoice =
        new_desk_trade_invoice(network.tenant(0, "buyer"), 1_000_000, preimage, false).await;
    let hash = *invoice.payment_hash();
    let desk = &network.nodes[1];
    let hold = new_desk_hold_invoice(desk, 1_000_000, hash);
    seller_pays_the_desk_hold(&network, &hold).await;
    desk.settle_invoice(&hash, preimage).await.unwrap();
    network
        .tenant(0, "seller")
        .node
        .wait_until_success(hash)
        .await;
    let before = network.nodes[0].store.get_payment_session(hash).unwrap();
    assert_eq!(before.status, PaymentStatus::Success);
    desk_pays_the_buyer(&network, &invoice).await;
    desk.wait_until_success(hash).await;
    wait_for_delivery_status(
        &network.lsp(0).client,
        hash,
        LspPaymentDeliveryStatus::Succeeded,
    )
    .await;
    let after = network.nodes[0].store.get_payment_session(hash).unwrap();
    assert_eq!(
        bincode::serialize(&before).unwrap(),
        bincode::serialize(&after).unwrap(),
        "hosted receipt must not overwrite the completed public session"
    );
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_cancellation_does_not_cancel_same_hash_outgoing_payment() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let buyer = network.tenant(0, "buyer");
    let invoice = new_desk_trade_invoice(buyer, 1_000_000, gen_rand_sha256_hash(), true).await;
    let hash = *invoice.payment_hash();
    let desk = &network.nodes[1];
    seller_pays_the_desk_hold(&network, &new_desk_hold_invoice(desk, 1_000_000, hash)).await;
    desk_pays_the_buyer(&network, &invoice).await;
    wait_until_async_timeout(|| async {
        buyer.node.get_invoice_status(&hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    buyer.node.cancel_invoice(&hash);
    desk.wait_until_failed(hash).await;
    wait_until_async_timeout(|| async {
        hosted_deliveries(&network.nodes[0], hash)
            .first()
            .is_some_and(|d| matches!(d.status, DeliveryStatus::Failed { .. }))
    })
    .await;
    let delivery = hosted_deliveries(&network.nodes[0], hash).remove(0);
    assert_eq!(delivery.attempt_count, 1);
    assert_eq!(
        delivery.last_error_code,
        Some(TlcErrorCode::InvoiceCancelled)
    );
    assert_eq!(
        network.nodes[0].get_payment_status(hash).await,
        PaymentStatus::Inflight
    );
    assert_eq!(
        network
            .tenant(0, "seller")
            .node
            .get_payment_status(hash)
            .await,
        PaymentStatus::Inflight
    );
    assert_eq!(
        desk.get_invoice_status(&hash),
        Some(CkbInvoiceStatus::Received)
    );
    desk.cancel_invoice(&hash);
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expiring_deferred_delivery_does_not_touch_same_hash_public_session() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let buyer = network.tenant(0, "buyer");
    let invoice = new_desk_trade_invoice(buyer, 1_000_000, gen_rand_sha256_hash(), false).await;
    let hash = *invoice.payment_hash();
    let desk = &network.nodes[1];
    seller_pays_the_desk_hold(&network, &new_desk_hold_invoice(desk, 1_000_000, hash)).await;
    // Exercise the real in-process disconnect and its channel-offline notification.
    disconnect_in_process(&network.nodes[0], &buyer.node);
    wait_for_tenant_channel(&network.lsp(0).client, &buyer.tenant_id, false).await;
    desk_pays_the_buyer(&network, &invoice).await;
    wait_for_delivery_status(
        &network.lsp(0).client,
        hash,
        LspPaymentDeliveryStatus::Deferred,
    )
    .await;
    let mut delivery = hosted_deliveries(&network.nodes[0], hash).remove(0);
    assert_eq!(delivery.attempt_count, 0);
    delivery.buffer_deadline = crate::now_timestamp_as_millis_u64() - 1;
    network.nodes[0]
        .store
        .namespaced(NodeNamespace::lsp_metadata())
        .put_lsp_payment_delivery(&delivery)
        .unwrap();
    network
        .lsp(0)
        .actor
        .send_message(LspServiceMessage::ExpireDelivery(delivery.key()))
        .unwrap();
    desk.wait_until_failed(hash).await;
    wait_for_delivery_status(
        &network.lsp(0).client,
        hash,
        LspPaymentDeliveryStatus::Failed,
    )
    .await;
    assert!(network.nodes[0]
        .get_channel_actor_state(buyer.private_channel_id)
        .tlc_state
        .offered_tlcs
        .tlcs
        .iter()
        .all(|tlc| tlc.payment_hash != hash));
    assert_eq!(
        network.nodes[0].get_payment_status(hash).await,
        PaymentStatus::Inflight
    );
    assert_eq!(
        network
            .tenant(0, "seller")
            .node
            .get_payment_status(hash)
            .await,
        PaymentStatus::Inflight
    );
    assert_eq!(
        buyer.node.get_invoice_status(&hash),
        Some(CkbInvoiceStatus::Open)
    );
    assert_eq!(
        desk.get_invoice_status(&hash),
        Some(CkbInvoiceStatus::Received)
    );
    desk.cancel_invoice(&hash);
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_same_hash_session_does_not_affect_inflight_delivery_recovery() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let public = &network.nodes[0];
    let desk = &network.nodes[1];
    let buyer = network.tenant(0, "buyer");
    let amount = 1_000_000;
    let preimage = gen_rand_sha256_hash();
    let invoice = new_desk_trade_invoice(buyer, amount, preimage, true).await;
    let hash = *invoice.payment_hash();
    seller_pays_the_desk_hold(&network, &new_desk_hold_invoice(desk, amount, hash)).await;
    let balance_before = buyer
        .node
        .get_local_balance_from_channel(buyer.private_channel_id);
    desk_pays_the_buyer(&network, &invoice).await;
    wait_until_async_timeout(|| async {
        buyer.node.get_invoice_status(&hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    let delivery = wait_for_buyer_delivery(&network, hash).await;
    let previous = delivery.request.previous_tlc;
    let channel = public.get_channel_actor_state(buyer.private_channel_id);
    let original_id = channel.find_hosted_tlc(hash, &previous).unwrap().tlc_id;
    let next_tlc_id = channel.tlc_state.offered_tlcs.next_tlc_id;

    desk.cancel_invoice(&hash);
    network
        .tenant(0, "seller")
        .node
        .wait_until_failed(hash)
        .await;
    public.wait_until_failed(hash).await;
    // Allow the unrelated failure callback and delivery retry loop to run while
    // the buyer still holds its TLC. Neither may settle or redispatch this payment.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(hosted_deliveries(public, hash).len(), 1);
    assert_eq!(
        hosted_deliveries(public, hash)[0].status,
        DeliveryStatus::InFlight
    );
    assert_eq!(
        buyer.node.get_invoice_status(&hash),
        Some(CkbInvoiceStatus::Received)
    );
    redispatch(&network, &delivery).await;

    // Model a crash after dispatch persisted the session but before the service
    // persisted InFlight. Recovery must select the hosted session, not the failed
    // hash-only session, and reuse its existing downstream TLC.
    let mut interrupted = delivery.clone();
    interrupted.status = DeliveryStatus::Dispatching;
    public
        .store
        .namespaced(NodeNamespace::lsp_metadata())
        .put_lsp_payment_delivery(&interrupted)
        .unwrap();
    network
        .lsp(0)
        .actor
        .send_message(LspServiceMessage::ResumeDelivery(delivery.key()))
        .unwrap();
    wait_for_buyer_delivery(&network, hash).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        hosted_deliveries(public, hash)[0].status,
        DeliveryStatus::InFlight
    );
    assert_eq!(
        buyer.node.get_invoice_status(&hash),
        Some(CkbInvoiceStatus::Received)
    );
    let restored = public.get_channel_actor_state(buyer.private_channel_id);
    assert_eq!(
        restored.find_hosted_tlc(hash, &previous).unwrap().tlc_id,
        original_id
    );
    assert_eq!(restored.tlc_state.offered_tlcs.next_tlc_id, next_tlc_id);

    buyer.node.settle_invoice(&hash, preimage).await.unwrap();
    desk.wait_until_success(hash).await;
    wait_for_delivery_status(
        &network.lsp(0).client,
        hash,
        LspPaymentDeliveryStatus::Succeeded,
    )
    .await;
    wait_until_async_timeout(|| async {
        buyer
            .node
            .get_local_balance_from_channel(buyer.private_channel_id)
            == balance_before + amount
    })
    .await;
    assert_eq!(
        public.store.get_payment_session(hash).unwrap().status,
        PaymentStatus::Failed,
        "the buyer's success must not overwrite the seller's failed session"
    );
    assert_eq!(
        public
            .store
            .get_payment_session(delivery.request.hosted_session_key())
            .unwrap()
            .status,
        PaymentStatus::Success
    );
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_dispatch_and_lost_reply_reuse_the_same_downstream_tlc() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let buyer = network.tenant(0, "buyer");
    let preimage = gen_rand_sha256_hash();
    let invoice = new_desk_trade_invoice(buyer, 1_000_000, preimage, true).await;
    let hash = *invoice.payment_hash();
    desk_pays_the_buyer(&network, &invoice).await;
    wait_until_async_timeout(|| async {
        buyer.node.get_invoice_status(&hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    let delivery = wait_for_buyer_delivery(&network, hash).await;
    let previous = delivery.request.previous_tlc;
    let public = &network.nodes[0];
    let id = public
        .get_channel_actor_state(delivery.private_channel_id)
        .find_hosted_tlc(hash, &previous)
        .unwrap()
        .tlc_id;
    let count = public
        .get_channel_actor_state(buyer.private_channel_id)
        .tlc_state
        .offered_tlcs
        .next_tlc_id;
    for _ in 0..3 {
        redispatch(&network, &delivery).await;
    }
    // Simulate a lost dispatch reply: the persisted session owns the same TLC.
    redispatch(&network, &delivery).await;
    let restored = public.get_channel_actor_state(delivery.private_channel_id);
    assert_eq!(
        restored.find_hosted_tlc(hash, &previous).unwrap().tlc_id,
        id
    );
    assert_eq!(
        public
            .get_channel_actor_state(buyer.private_channel_id)
            .tlc_state
            .offered_tlcs
            .next_tlc_id,
        count
    );
    buyer.node.settle_invoice(&hash, preimage).await.unwrap();
    network.nodes[1].wait_until_success(hash).await;
    wait_for_delivery_status(
        &network.lsp(0).client,
        hash,
        LspPaymentDeliveryStatus::Succeeded,
    )
    .await;
    assert!(public.store.get_payment_session(hash).is_none());
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_while_service_is_stopped_retains_the_exact_outcome() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let buyer = network.tenant(0, "buyer");
    let preimage = gen_rand_sha256_hash();
    let invoice = new_desk_trade_invoice(buyer, 1_000_000, preimage, true).await;
    let hash = *invoice.payment_hash();
    desk_pays_the_buyer(&network, &invoice).await;
    wait_until_async_timeout(|| async {
        buyer.node.get_invoice_status(&hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    let delivery = wait_for_buyer_delivery(&network, hash).await;
    // The payment result must be durable even while its service is down.
    // Upstream settlement resumes after the service recovers.
    let service = network.lsp(0).actor.clone();
    service.stop(None);
    wait_until_async_timeout(|| async { service.get_status() == ractor::ActorStatus::Stopped })
        .await;
    let public = &network.nodes[0];
    buyer.node.settle_invoice(&hash, preimage).await.unwrap();
    wait_until_async_timeout(|| async {
        public
            .store
            .get_payment_session(delivery.request.hosted_session_key())
            .is_some_and(|outcome| outcome.status == PaymentStatus::Success)
    })
    .await;

    assert!(public.store.get_payment_session(hash).is_none());
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_incoming_execution_is_not_reported_inflight() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let buyer = network.tenant(0, "buyer");
    let invoice = new_desk_trade_invoice(buyer, 1_000_000, gen_rand_sha256_hash(), true).await;
    let hash = *invoice.payment_hash();
    desk_pays_the_buyer(&network, &invoice).await;
    let delivery = wait_for_buyer_delivery(&network, hash).await;
    let public = &network.nodes[0];
    let previous = delivery.request.previous_tlc;
    let mut closed = public.get_channel_actor_state(delivery.private_channel_id);
    closed.id = gen_rand_sha256_hash();
    closed.state =
        crate::fiber_types::ChannelState::Closed(crate::fiber_types::CloseFlags::empty());
    closed.tlc_state.offered_tlcs.tlcs.clear();
    let closed_id = closed.id;
    public.store.insert_channel_actor_state(closed);
    // A different incoming execution must not find this payment session.
    let response = ractor::call_t!(
        public.network_actor,
        |reply| NetworkActorMessage::new_command(FiberActorCommand::GetPayment(
            PaymentSessionKey {
                payment_hash: hash,
                upstream_tlc_key: Some(UpstreamTlcKey {
                    incoming_channel_id: closed_id,
                    incoming_tlc_id: previous.prev_tlc_id
                })
            },
            reply
        )),
        5_000
    )
    .unwrap();
    assert!(response.is_err());
    assert!(public
        .get_channel_actor_state(delivery.private_channel_id)
        .find_hosted_tlc(hash, &previous)
        .is_some());
    assert!(public.store.get_payment_session(hash).is_none());
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downstream_fulfill_is_scoped_to_the_hosted_execution() {
    check_downstream_outcome_isolation(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downstream_failure_is_scoped_to_the_hosted_execution() {
    check_downstream_outcome_isolation(false).await;
}

async fn check_downstream_outcome_isolation(fulfill: bool) {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let buyer = network.tenant(0, "buyer");
    let preimage = gen_rand_sha256_hash();
    let invoice = new_desk_trade_invoice(buyer, 1_000_000, preimage, true).await;
    let hash = *invoice.payment_hash();
    seller_pays_the_desk_hold(
        &network,
        &new_desk_hold_invoice(&network.nodes[1], 1_000_000, hash),
    )
    .await;
    desk_pays_the_buyer(&network, &invoice).await;
    let delivery = wait_for_buyer_delivery(&network, hash).await;
    let service = network.lsp(0).actor.clone();
    service.stop(None);
    wait_until_async_timeout(|| async { service.get_status() == ractor::ActorStatus::Stopped })
        .await;
    let public = &network.nodes[0];
    let previous = delivery.request.previous_tlc;
    let id = u64::from(
        public
            .get_channel_actor_state(delivery.private_channel_id)
            .find_hosted_tlc(hash, &previous)
            .unwrap()
            .tlc_id,
    );
    // Exercise payment result routing, not CKB proof verification.
    let reason = if fulfill {
        RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill {
            payment_preimage: preimage,
        })
    } else {
        let received = buyer
            .node
            .get_channel_actor_state(buyer.private_channel_id)
            .tlc_state
            .received_tlcs
            .tlcs
            .iter()
            .find(|tlc| tlc.payment_hash == hash)
            .unwrap()
            .clone();
        RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
            TlcErr::new_node_fail(TlcErrorCode::PermanentNodeFailure, buyer.node.pubkey),
            &received.shared_secret,
        ))
    };
    public
        .network_actor
        .send_message(NetworkActorMessage::new_event(
            crate::fiber::network::FiberActorEvent::TlcRemoveReceived(
                delivery.request.hosted_session_key(),
                public
                    .get_channel_actor_state(delivery.private_channel_id)
                    .tlc_state
                    .get(&TLCId::Offered(id))
                    .unwrap()
                    .attempt_id,
                reason,
                None,
            ),
        ))
        .unwrap();
    wait_until_async_timeout(|| async {
        public
            .store
            .get_payment_session(delivery.request.hosted_session_key())
            .is_some_and(|session| session.status.is_final())
    })
    .await;
    let payment = ractor::call_t!(
        public.network_actor,
        |reply| NetworkActorMessage::new_command(FiberActorCommand::GetPayment(
            delivery.request.hosted_session_key(),
            reply
        )),
        5_000
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        payment.status,
        if fulfill {
            PaymentStatus::Success
        } else {
            PaymentStatus::Failed
        }
    );
    if !fulfill {
        assert_eq!(
            payment.failed_error_code,
            Some(TlcErrorCode::PermanentNodeFailure)
        );

        assert_eq!(
            public.get_payment_status(hash).await,
            PaymentStatus::Inflight
        );
        assert_eq!(
            network
                .tenant(0, "seller")
                .node
                .get_payment_status(hash)
                .await,
            PaymentStatus::Inflight
        );
    }
    // Fulfill may legitimately reveal the preimage to the other payment. Its
    // destination and context must nevertheless remain its original desk invoice.
    assert_eq!(
        public
            .store
            .get_payment_session(hash)
            .unwrap()
            .request
            .target_pubkey,
        network.nodes[1].pubkey
    );
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_hash_delivery_survives_public_node_and_service_restart() {
    init_tracing();
    let mut network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let preimage = gen_rand_sha256_hash();
    let invoice =
        new_desk_trade_invoice(network.tenant(0, "buyer"), 1_000_000, preimage, true).await;
    let hash = *invoice.payment_hash();
    seller_pays_the_desk_hold(
        &network,
        &new_desk_hold_invoice(&network.nodes[1], 1_000_000, hash),
    )
    .await;
    desk_pays_the_buyer(&network, &invoice).await;
    wait_until_async_timeout(|| async {
        network.tenant(0, "buyer").node.get_invoice_status(&hash)
            == Some(CkbInvoiceStatus::Received)
    })
    .await;
    let delivery = wait_for_buyer_delivery(&network, hash).await;
    let previous = delivery.request.previous_tlc;
    let original_id = network.nodes[0]
        .get_channel_actor_state(delivery.private_channel_id)
        .find_hosted_tlc(hash, &previous)
        .unwrap()
        .tlc_id;

    let old_service = network.lsp(0).actor.clone();
    // These fixture tenants are independent NetworkNodes, unlike production's
    // co-located actors. Explicitly model loss of their in-process route to T.
    for tenant in &network.tenants {
        disconnect_in_process(&network.nodes[0], &tenant.node);
    }
    old_service.stop(Some("restart recovery test".into()));
    wait_until_async_timeout(|| async { old_service.get_status() == ractor::ActorStatus::Stopped })
        .await;
    tokio::time::timeout(Duration::from_secs(10), network.nodes[0].restart())
        .await
        .expect("restart Public T");
    let (public, others) = network.nodes.split_at_mut(1);
    public[0].connect_to_nonblocking(&others[0]).await;
    for tenant in &network.tenants {
        connect_in_process(&network.nodes[0], &tenant.node).await;
    }

    let public = &network.nodes[0];
    let registry =
        crate::lsp::TenantRegistry::new(public.store.namespaced(NodeNamespace::lsp_metadata()));
    let runtimes = network
        .tenants
        .iter()
        .map(|tenant| {
            (
                tenant.tenant_id.clone(),
                ExistingRuntime {
                    record: registry.get(&tenant.tenant_id).unwrap().unwrap(),
                    network_actor: tenant.node.network_actor.clone(),
                    rpc_context: None,
                },
            )
        })
        .collect();
    let token_key = KeyPair::new();
    let service = Actor::spawn(
        None,
        LspService,
        LspServiceArgs {
            config: lsp_config(network._root.path().join("lsp-0")),
            public_node_id: public.pubkey,
            public_network_actor: public.network_actor.clone(),
            store: public.store.namespaced(NodeNamespace::lsp_metadata()),
            runtime_factory: Arc::new(ExistingRuntimeFactory {
                runtimes,
                starts: Arc::new(AtomicUsize::new(0)),
            }),
            signing_key: public.private_key.clone(),
            token_issuer: BiscuitTokenIssuer::from_private_key(
                &token_key.private().to_prefixed_string(),
                &token_key.public().to_string(),
            )
            .unwrap(),
            watchtower_store: public.store.clone(),
            signing_events: public.signing_events.clone(),
        },
    )
    .await
    .unwrap()
    .0;
    public
        .network_actor
        .send_message(NetworkActorMessage::new_command(
            PublicNetworkCommand::SetDeliveryPort(crate::lsp::LspDeliveryPort::new(
                service.clone(),
            )),
        ))
        .unwrap();
    network.lsp_services[0].actor = service;
    assert_eq!(
        network.nodes[0]
            .get_channel_actor_state(delivery.private_channel_id)
            .find_hosted_tlc(hash, &previous)
            .unwrap()
            .tlc_id,
        original_id
    );
    let buyer = network.tenant(0, "buyer");
    buyer.node.settle_invoice(&hash, preimage).await.unwrap();
    network.nodes[1].wait_until_success(hash).await;
    wait_until_async_timeout(|| async {
        hosted_deliveries(&network.nodes[0], hash)[0].status == DeliveryStatus::Succeeded
    })
    .await;
    assert_eq!(
        network.nodes[0]
            .store
            .get_payment_session(delivery.request.hosted_session_key())
            .unwrap()
            .status,
        PaymentStatus::Success
    );
    assert_eq!(
        network.nodes[0]
            .store
            .get_payment_session(hash)
            .unwrap()
            .request
            .target_pubkey,
        network.nodes[1].pubkey
    );
    settle_desk_hold(&network.nodes[1], hash, preimage).await;
    network
        .tenant(0, "seller")
        .node
        .wait_until_success(hash)
        .await;
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_sellers_hold_must_not_fail_the_buyers_deferred_delivery() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let desk = &network.nodes[1];
    let public_t = &network.nodes[0];
    let buyer = network.tenant(0, "buyer");
    let buyer_invoice =
        new_desk_trade_invoice(buyer, 1_000_000, gen_rand_sha256_hash(), false).await;
    let hash = *buyer_invoice.payment_hash();
    let hold = new_desk_hold_invoice(desk, 1_000_000, hash);
    seller_pays_the_desk_hold(&network, &hold).await;
    // Keep B deferred even when the seller's actor stops, so its callback
    // cannot be confused with a genuine new T -> B dispatch result.
    network
        .lsp(0)
        .actor
        .send_message(LspServiceMessage::TenantChannelOffline(
            buyer.node.pubkey,
            buyer.private_channel_id,
        ))
        .unwrap();
    desk_pays_the_buyer(&network, &buyer_invoice).await;
    wait_until_async_timeout(|| async {
        hosted_deliveries(public_t, hash).first().is_some_and(|d| {
            d.status == crate::lsp::LspPaymentDeliveryStatus::Deferred && d.attempt_count == 0
        })
    })
    .await;
    desk.cancel_invoice(&hash);
    network
        .tenant(0, "seller")
        .node
        .wait_until_failed(hash)
        .await;
    let deliveries = hosted_deliveries(public_t, hash);
    assert_eq!(deliveries[0].attempt_count, 0);
    assert_eq!(
        deliveries[0].status,
        crate::lsp::LspPaymentDeliveryStatus::Deferred,
        "an unrelated outgoing session's cancellation must not settle B's incoming delivery"
    );
    network.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unrelated_success_does_not_complete_same_hash_deferred_delivery() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let buyer = network.tenant(0, "buyer");
    let preimage = gen_rand_sha256_hash();
    let invoice = new_desk_trade_invoice(buyer, 1_000_000, preimage, false).await;
    let hash = *invoice.payment_hash();
    let desk = &network.nodes[1];
    seller_pays_the_desk_hold(&network, &new_desk_hold_invoice(desk, 1_000_000, hash)).await;
    network
        .lsp(0)
        .actor
        .send_message(LspServiceMessage::TenantChannelOffline(
            buyer.node.pubkey,
            buyer.private_channel_id,
        ))
        .unwrap();
    desk_pays_the_buyer(&network, &invoice).await;
    wait_until_async_timeout(|| async { !hosted_deliveries(&network.nodes[0], hash).is_empty() })
        .await;
    settle_desk_hold(desk, hash, preimage).await;
    network
        .tenant(0, "seller")
        .node
        .wait_until_success(hash)
        .await;
    ractor::call!(network.lsp(0).actor, LspServiceMessage::GetStatus).unwrap();
    let delivery = hosted_deliveries(&network.nodes[0], hash).remove(0);
    assert_eq!(delivery.status, DeliveryStatus::Deferred);
    assert_eq!(delivery.attempt_count, 0);
    assert_eq!(
        buyer.node.get_invoice_status(&hash),
        Some(CkbInvoiceStatus::Open)
    );
    assert_eq!(desk.get_payment_status(hash).await, PaymentStatus::Inflight);
    network.stop().await;
}
use crate::fiber::graph::NetworkGraphStateStore as _;

/// A hosted tenant's invoice for the hash of `preimage`: settled with it at once, or held until
/// the test settles it.
async fn new_desk_trade_invoice(
    tenant: &HostedTenantTestNode,
    amount: u128,
    preimage: crate::fiber_types::Hash256,
    hold: bool,
) -> crate::invoice::CkbInvoice {
    let payment_hash: crate::fiber_types::Hash256 =
        HashAlgorithm::CkbHash.hash(preimage.as_ref()).into();
    let invoice: fiber_json_types::InvoiceResult = tenant
        .client
        .request(
            "new_invoice",
            rpc_params![fiber_json_types::NewInvoiceParams {
                amount,
                description: Some("a buyer's invoice on a P2P desk".to_string()),
                currency: fiber_json_types::Currency::Fibd,
                payment_preimage: (!hold).then(|| preimage.into()),
                payment_hash: hold.then(|| payment_hash.into()),
                expiry: Some(60 * 60),
                fallback_address: None,
                final_expiry_delta: Some(60_000),
                udt_type_script: None,
                hash_algorithm: None,
                allow_mpp: Some(false),
                allow_trampoline_routing: Some(true),
            }],
        )
        .await
        .expect("create the buyer's hosted invoice");
    let invoice = crate::invoice::CkbInvoice::from_str(&invoice.invoice_address)
        .expect("decode the buyer's invoice");
    assert_eq!(*invoice.payment_hash(), payment_hash);
    invoice
}

/// The desk's hold invoice for a buyer's hash: no preimage, it settles once the buyer is paid.
fn new_desk_hold_invoice(
    desk: &NetworkNode,
    amount: u128,
    payment_hash: crate::fiber_types::Hash256,
) -> crate::invoice::CkbInvoice {
    let invoice = InvoiceBuilder::new(Currency::Fibd)
        .amount(Some(amount))
        .payment_hash(payment_hash)
        .final_expiry_delta(60_000)
        .allow_trampoline_routing(true)
        .payee_pub_key(desk.pubkey.into())
        .description("the desk's hold invoice for the buyer's hash".to_string())
        .payment_secret(gen_rand_sha256_hash())
        .build_with_sign(|message| SECP256K1.sign_ecdsa_recoverable(message, &desk.private_key.0))
        .expect("build the desk's hold invoice");
    desk.insert_invoice(invoice.clone(), None);
    invoice
}

async fn payment_end(
    node: &NetworkNode,
    payment_hash: crate::fiber_types::Hash256,
) -> PaymentStatus {
    let mut status = PaymentStatus::Created;
    for _ in 0..120 {
        status = node.get_payment_status(payment_hash).await;
        if matches!(status, PaymentStatus::Success | PaymentStatus::Failed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    status
}

fn hosted_deliveries(
    public_t: &NetworkNode,
    payment_hash: crate::fiber_types::Hash256,
) -> Vec<crate::lsp::LspPaymentDelivery> {
    public_t
        .store
        .namespaced(NodeNamespace::lsp_metadata())
        .list_lsp_payment_deliveries_by_payment_hash(&payment_hash)
        .expect("read hosted deliveries")
}

/// The desk of a P2P exchange and two tenants of Public T trading through it:
///
/// Hosted seller S -private- Public T -private- Hosted buyer B
///                              |
///                           public
///                              |
///                           Desk R
///
/// One hash runs end to end: B makes an invoice for H, R makes a hold invoice for the same H,
/// S pays it through T and R holds it; once the trade clears, R pays B's invoice through T and
/// settles its hold with the preimage B reveals.
async fn create_desk_trade_network(buyer_lsp_funding: u128) -> LspTestNetwork {
    let network = create_lsp_test_network(
        &[((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT), true)],
        2,
        &[
            ((0, "seller"), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT)),
            ((0, "buyer"), (buyer_lsp_funding, HUGE_CKB_AMOUNT)),
        ],
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    network
}

/// S pays R's hold invoice through T, and R holds it: T now has a payment session for H, S's.
async fn seller_pays_the_desk_hold(network: &LspTestNetwork, hold: &crate::invoice::CkbInvoice) {
    let public_t = &network.nodes[0];
    let desk = &network.nodes[1];
    let payment_hash = *hold.payment_hash();
    network
        .send_payment(
            0,
            "seller",
            SendPaymentCommand {
                invoice: Some(hold.to_string()),
                max_fee_amount: Some(2_000_000),
                trampoline_hops: Some(vec![public_t.pubkey]),
                ..Default::default()
            },
        )
        .await
        .expect("the seller pays the desk's hold invoice");
    wait_until_async_timeout(|| async {
        desk.get_invoice_status(&payment_hash) == Some(CkbInvoiceStatus::Received)
    })
    .await;
    let session = public_t
        .store
        .get_payment_session(payment_hash)
        .expect("T forwards the seller's payment with a session for H");
    assert_eq!(session.request.target_pubkey, desk.pubkey);
}

async fn desk_pays_the_buyer(network: &LspTestNetwork, invoice: &crate::invoice::CkbInvoice) {
    network.nodes[1]
        .send_payment(SendPaymentCommand {
            invoice: Some(invoice.to_string()),
            max_fee_amount: Some(2_000_000),
            trampoline_hops: Some(vec![network.nodes[0].pubkey]),
            ..Default::default()
        })
        .await
        .expect("the desk pays the buyer's invoice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_is_paid_with_a_hash_its_lsp_already_pays_elsewhere() {
    init_tracing();

    // The hosted last hop must not create a second hash-keyed session on T.
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let public_t = &network.nodes[0];
    let desk = &network.nodes[1];
    let buyer = network.tenant(0, "buyer");
    let amount = 1_000_000;
    let preimage = gen_rand_sha256_hash();
    let buyer_invoice = new_desk_trade_invoice(buyer, amount, preimage, false).await;
    let payment_hash = *buyer_invoice.payment_hash();
    let hold = new_desk_hold_invoice(desk, amount, payment_hash);
    seller_pays_the_desk_hold(&network, &hold).await;

    let buyer_before = buyer
        .node
        .get_local_balance_from_channel(buyer.private_channel_id);
    desk_pays_the_buyer(&network, &buyer_invoice).await;
    let status = payment_end(desk, payment_hash).await;
    assert_eq!(
        status,
        PaymentStatus::Success,
        "the desk's payment to the buyer ended {status:?}"
    );
    assert_eq!(
        buyer.node.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Paid)
    );
    let learned = desk
        .get_payment_result(payment_hash)
        .await
        .payment_preimage
        .expect("the desk learns the buyer's preimage");
    assert_eq!(learned, preimage);

    settle_desk_hold(desk, payment_hash, learned).await;
    network
        .tenant(0, "seller")
        .node
        .wait_until_success(payment_hash)
        .await;
    wait_for_delivery_status(
        &network.lsp(0).client,
        payment_hash,
        LspPaymentDeliveryStatus::Succeeded,
    )
    .await;
    assert_eq!(hosted_deliveries(public_t, payment_hash).len(), 1);
    wait_until_async_timeout(|| async {
        buyer
            .node
            .get_local_balance_from_channel(buyer.private_channel_id)
            == buyer_before + amount
    })
    .await;
    assert_eq!(
        public_t
            .store
            .get_payment_session(payment_hash)
            .map(|session| (session.request.target_pubkey, session.status)),
        Some((desk.pubkey, PaymentStatus::Success)),
        "T's session for H is still the seller's, now paid"
    );

    network.stop().await;
}

// Controls: the same flows with the seller's hold on another hash.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_a_tenant_is_paid_while_its_lsp_pays_another_hash_elsewhere() {
    init_tracing();
    let network = create_desk_trade_network(HUGE_CKB_AMOUNT).await;
    let desk = &network.nodes[1];
    let buyer = network.tenant(0, "buyer");
    let amount = 1_000_000;
    let buyer_invoice = new_desk_trade_invoice(buyer, amount, gen_rand_sha256_hash(), false).await;
    let payment_hash = *buyer_invoice.payment_hash();
    let other_hash = gen_rand_sha256_hash();
    let hold = new_desk_hold_invoice(desk, amount, other_hash);
    seller_pays_the_desk_hold(&network, &hold).await;

    desk_pays_the_buyer(&network, &buyer_invoice).await;
    let status = payment_end(desk, payment_hash).await;
    assert_eq!(
        status,
        PaymentStatus::Success,
        "control: the desk's payment ended {status:?}"
    );
    assert_eq!(
        buyer.node.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Paid)
    );
    desk.cancel_invoice(&other_hash);
    network.stop().await;
}
