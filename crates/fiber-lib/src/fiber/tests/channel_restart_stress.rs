#[cfg(not(target_arch = "wasm32"))]
use crate::fiber::payment::{SendPaymentCommand, SendPaymentWithRouterCommand};
use crate::fiber::types::Hash256;
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::invoice::NewInvoiceParams;
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::peer::{ConnectPeerParams, DisconnectPeerParams};
use crate::test_utils::{init_tracing, NetworkNode};
use crate::tests::test_utils::{
    create_n_nodes_network, create_n_nodes_network_with_params,
    create_nodes_with_established_channel, gen_rpc_config, ChannelParameters, HUGE_CKB_AMOUNT,
    MIN_RESERVED_CKB,
};
use fiber_types::{AttemptStatus, PaymentStatus, TlcErrorCode};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, error};

/// Restart stress reproducer.
/// Runs repeated payment bursts with node restart and checks unexpected events.
#[tokio::test]
#[ignore] // Long-running restart stress test. Run explicitly when validating restart regressions.
async fn test_node_restart() {
    init_tracing();
    let (mut node_a, node_b, channel_id) =
        create_nodes_with_established_channel(100000000000, 100000000000, true).await;
    let panic_unexpected_events = vec!["panic".to_string(), "panicked".to_string()];
    node_a
        .add_unexpected_events(panic_unexpected_events.clone())
        .await;
    node_b
        .add_unexpected_events(panic_unexpected_events.clone())
        .await;

    for cycle in 0..10 {
        debug!("=== Restart cycle {} ===", cycle);

        for _i in 0..10 {
            let _payment1 = node_a.send_payment_keysend(&node_b, 1, false).await;
            let _payment2 = node_b.send_payment_keysend(&node_a, 1, false).await;
        }

        let node_a_unexpected_events = node_a.get_triggered_unexpected_events().await;
        assert!(
            node_a_unexpected_events.is_empty(),
            "node_a got unexpected events before restart cycle {}: {:?}",
            cycle,
            node_a_unexpected_events
        );
        let node_b_unexpected_events = node_b.get_triggered_unexpected_events().await;
        assert!(
            node_b_unexpected_events.is_empty(),
            "node_b got unexpected events before restart cycle {}: {:?}",
            cycle,
            node_b_unexpected_events
        );

        debug!("Stopping node_a for restart cycle {}", cycle);
        node_a.restart().await;
        debug!("Starting node_a after restart cycle {}", cycle);
        node_a
            .add_unexpected_events(panic_unexpected_events.clone())
            .await;

        tokio::time::sleep(Duration::from_secs(10)).await;

        let node_a_unexpected_events = node_a.get_triggered_unexpected_events().await;
        assert!(
            node_a_unexpected_events.is_empty(),
            "node_a got unexpected events after restart cycle {}: {:?}",
            cycle,
            node_a_unexpected_events
        );
        let node_b_unexpected_events = node_b.get_triggered_unexpected_events().await;
        assert!(
            node_b_unexpected_events.is_empty(),
            "node_b got unexpected events after restart cycle {}: {:?}",
            cycle,
            node_b_unexpected_events
        );
    }

    // Verify no stuck TLCs after all restart cycles.
    let state_a = node_a.get_channel_actor_state(channel_id);
    let state_b = node_b.get_channel_actor_state(channel_id);
    let tlc_count_a = state_a.tlc_state.all_tlcs().count();
    let tlc_count_b = state_b.tlc_state.all_tlcs().count();
    assert_eq!(
        tlc_count_a, 0,
        "node_a channel {:?} still has {} stuck TLCs",
        channel_id, tlc_count_a
    );
    assert_eq!(
        tlc_count_b, 0,
        "node_b channel {:?} still has {} stuck TLCs",
        channel_id, tlc_count_b
    );

    debug!("test_node_restart completed successfully with no unexpected events");
}

/// Reproducer: 4-node ring (A-B-C-D-A), all nodes send 100 self-payments
/// through the ring, then restart A and D.
/// Expected: all TLCs settle, channel balances conserved (only routing fees change).
#[tokio::test]
#[ignore] // Long-running stress test. Run explicitly to reproduce stuck-TLC bug.
async fn test_ring_self_payments_then_restart_two_nodes() {
    init_tracing();

    #[derive(Debug, Clone)]
    struct RingTlcTrace {
        sender: &'static str,
        sender_idx: usize,
        seq: u32,
        payment_hash: Hash256,
        request_amount: u128,
        initial_status: PaymentStatus,
        route_channel_outpoints: Vec<String>,
    }

    // Build ring topology: A(0)-B(1)-C(2)-D(3)-A(0)
    let funding = HUGE_CKB_AMOUNT;
    let (nodes, channels) = create_n_nodes_network(
        &[
            ((0, 1), (funding, funding)), // A-B
            ((1, 2), (funding, funding)), // B-C
            ((2, 3), (funding, funding)), // C-D
            ((3, 0), (funding, funding)), // D-A  (closes the ring)
        ],
        4,
    )
    .await;
    let [mut node_a, node_b, node_c, mut node_d] = nodes.try_into().expect("4 nodes");
    let mut send_seq = 0u32;
    let mut send_traces: Vec<RingTlcTrace> = Vec::new();

    let panic_events = vec!["panic".to_string(), "panicked".to_string()];
    node_a.add_unexpected_events(panic_events.clone()).await;
    node_b.add_unexpected_events(panic_events.clone()).await;
    node_c.add_unexpected_events(panic_events.clone()).await;
    node_d.add_unexpected_events(panic_events.clone()).await;

    // Channel layout:
    //   channels[0]: A-B  (node_a, node_b)
    //   channels[1]: B-C  (node_b, node_c)
    //   channels[2]: C-D  (node_c, node_d)
    //   channels[3]: D-A  (node_d, node_a)
    //
    // Each node participates in exactly 2 channels.
    // Record initial balances from each node's perspective on its channels.
    let initial_a_ch0 = node_a.get_local_balance_from_channel(channels[0]);
    let initial_a_ch3 = node_a.get_local_balance_from_channel(channels[3]);
    let initial_b_ch0 = node_b.get_local_balance_from_channel(channels[0]);
    let initial_b_ch1 = node_b.get_local_balance_from_channel(channels[1]);
    let initial_c_ch1 = node_c.get_local_balance_from_channel(channels[1]);
    let initial_c_ch2 = node_c.get_local_balance_from_channel(channels[2]);
    let initial_d_ch2 = node_d.get_local_balance_from_channel(channels[2]);
    let initial_d_ch3 = node_d.get_local_balance_from_channel(channels[3]);

    // For self-payments the sender == receiver, so net balance across each node's
    // two channels should stay the same (minus routing fees paid to intermediaries).
    // Total across all channels should be strictly conserved.
    let initial_total = initial_a_ch0
        + initial_a_ch3
        + initial_b_ch0
        + initial_b_ch1
        + initial_c_ch1
        + initial_c_ch2
        + initial_d_ch2
        + initial_d_ch3;

    // Fire off 100 self-payments from each node (fire-and-forget, don't wait)
    let payment_amount = 1000;
    let num_payments = 100u32;

    debug!(
        "=== Sending {} self-payments from each of 4 nodes ===",
        num_payments
    );
    for i in 0..num_payments {
        send_seq += 1;
        if let Ok(res) = node_a
            .send_payment_keysend_to_self(payment_amount, false)
            .await
        {
            send_traces.push(RingTlcTrace {
                sender: "A",
                sender_idx: 0,
                seq: send_seq,
                payment_hash: res.payment_hash,
                request_amount: payment_amount,
                initial_status: res.status,
                route_channel_outpoints: vec![],
            });
        }
        send_seq += 1;
        if let Ok(res) = node_b
            .send_payment_keysend_to_self(payment_amount, false)
            .await
        {
            send_traces.push(RingTlcTrace {
                sender: "B",
                sender_idx: 1,
                seq: send_seq,
                payment_hash: res.payment_hash,
                request_amount: payment_amount,
                initial_status: res.status,
                route_channel_outpoints: vec![],
            });
        }
        send_seq += 1;
        if let Ok(res) = node_c
            .send_payment_keysend_to_self(payment_amount, false)
            .await
        {
            send_traces.push(RingTlcTrace {
                sender: "C",
                sender_idx: 2,
                seq: send_seq,
                payment_hash: res.payment_hash,
                request_amount: payment_amount,
                initial_status: res.status,
                route_channel_outpoints: vec![],
            });
        }
        send_seq += 1;
        if let Ok(res) = node_d
            .send_payment_keysend_to_self(payment_amount, false)
            .await
        {
            send_traces.push(RingTlcTrace {
                sender: "D",
                sender_idx: 3,
                seq: send_seq,
                payment_hash: res.payment_hash,
                request_amount: payment_amount,
                initial_status: res.status,
                route_channel_outpoints: vec![],
            });
        }
        if i % 20 == 0 {
            debug!("Sent batch {}/{}", i, num_payments);
        }
    }

    {
        let source_nodes = [&node_a, &node_b, &node_c, &node_d];
        for trace in &mut send_traces {
            if let Some(session) =
                source_nodes[trace.sender_idx].get_payment_session(trace.payment_hash)
            {
                trace.initial_status = session.status;
                if let Some(attempt) = session.attempts().next() {
                    trace.route_channel_outpoints = attempt
                        .route
                        .nodes
                        .iter()
                        .map(|node| format!("{:?}", node.channel_outpoint))
                        .collect();
                }
            }
        }
    }
    debug!(
        "Pre-restart trace snapshot: total={} with_route={}",
        send_traces.len(),
        send_traces
            .iter()
            .filter(|trace| !trace.route_channel_outpoints.is_empty())
            .count()
    );

    tokio::time::sleep(Duration::from_secs(2)).await;

    for trace in &send_traces {
        debug!(
            "payment trace [{}] sender={} seq={} amount={} init_status={:?} path={}",
            trace.payment_hash,
            trace.sender,
            trace.seq,
            trace.request_amount,
            trace.initial_status,
            trace.route_channel_outpoints.join(" -> ")
        );
    }

    for (name, node) in [
        ("A", &node_a),
        ("B", &node_b),
        ("C", &node_c),
        ("D", &node_d),
    ] {
        let events = node.get_triggered_unexpected_events().await;
        assert!(
            events.is_empty(),
            "node {} got unexpected events before restart: {:?}",
            name,
            events
        );
    }

    debug!("=== Restarting node A and D ===");
    node_a.restart().await;
    node_d.restart().await;
    node_a.add_unexpected_events(panic_events.clone()).await;
    node_d.add_unexpected_events(panic_events.clone()).await;

    let chs_a = [channels[0], channels[3]];
    let chs_b = [channels[0], channels[1]];
    let chs_c = [channels[1], channels[2]];
    let chs_d = [channels[2], channels[3]];
    let checks: Vec<(&str, &NetworkNode, &[Hash256])> = vec![
        ("A", &node_a, &chs_a),
        ("B", &node_b, &chs_b),
        ("C", &node_c, &chs_c),
        ("D", &node_d, &chs_d),
    ];

    debug!("=== Waiting for reestablish and TLC settlement (timeout=300s) ===");
    let wait_start = tokio::time::Instant::now();
    let wait_timeout = Duration::from_secs(300);
    let wait_interval = Duration::from_millis(500);
    loop {
        let reestablished = chs_a
            .iter()
            .all(|ch| !node_a.get_channel_actor_state(*ch).reestablishing)
            && chs_d
                .iter()
                .all(|ch| !node_d.get_channel_actor_state(*ch).reestablishing);

        let tlcs_settled = checks.iter().all(|(_, node, chs)| {
            chs.iter().all(|ch| {
                node.get_channel_actor_state(*ch)
                    .tlc_state
                    .all_tlcs()
                    .next()
                    .is_none()
            })
        });

        if reestablished && tlcs_settled {
            debug!(
                "Reestablish and TLC settlement conditions reached after {:?}",
                wait_start.elapsed()
            );
            break;
        }

        if wait_start.elapsed() >= wait_timeout {
            debug!(
                "Waited {:?} without full settlement, continuing to assertions",
                wait_timeout
            );
            break;
        }

        tokio::time::sleep(wait_interval).await;
    }

    for (name, node) in [
        ("A", &node_a),
        ("B", &node_b),
        ("C", &node_c),
        ("D", &node_d),
    ] {
        let events = node.get_triggered_unexpected_events().await;
        assert!(
            events.is_empty(),
            "node {} got unexpected events after restart: {:?}",
            name,
            events
        );
    }

    for ch in [channels[0], channels[3]] {
        let state = node_a.get_channel_actor_state(ch);
        assert!(
            !state.reestablishing,
            "Node A channel {:?} still reestablishing",
            ch
        );
    }
    for ch in [channels[2], channels[3]] {
        let state = node_d.get_channel_actor_state(ch);
        assert!(
            !state.reestablishing,
            "Node D channel {:?} still reestablishing",
            ch
        );
    }

    let source_nodes = [&node_a, &node_b, &node_c, &node_d];
    let mut stuck_channels: Vec<(&str, Hash256, usize)> = vec![];
    for (name, node, chs) in &checks {
        for ch in *chs {
            let state = node.get_channel_actor_state(*ch);
            let tlcs = state.tlc_state.all_tlcs().collect::<Vec<_>>();
            let tlc_count = tlcs.len();
            for tlc in tlcs {
                let source = if source_nodes
                    .iter()
                    .any(|node| node.get_payment_session(tlc.payment_hash).is_some())
                {
                    "source-known"
                } else {
                    "source-unknown"
                };
                debug!(
                    "residual tlc on node={} channel={:?} payment_hash={:?} tlc_id={:?} status={:?} source_session={}",
                    name, ch, tlc.payment_hash, tlc.tlc_id, tlc.status, source
                );
            }
            if tlc_count > 0 {
                stuck_channels.push((*name, *ch, tlc_count));
            }
        }
    }
    let mut residual_by_hash = HashMap::new();
    for (name, node, chs) in &checks {
        for ch in *chs {
            let state = node.get_channel_actor_state(*ch);
            for tlc in state.tlc_state.all_tlcs() {
                residual_by_hash
                    .entry(tlc.payment_hash)
                    .or_insert_with(Vec::new)
                    .push((*name, *ch, tlc.tlc_id, tlc.status.clone()));
            }
        }
    }
    let residual_count = residual_by_hash.values().map(|x| x.len()).sum::<usize>();
    debug!(
        "Post-restart residual summary: unique_payment_hash={} total_tlc_entries={}",
        residual_by_hash.len(),
        residual_count
    );
    let mut trace_by_hash = HashMap::new();
    for trace in &send_traces {
        trace_by_hash.insert(trace.payment_hash, trace.clone());
    }
    let mut traces_with_no_residual = 0usize;
    let mut residual_without_trace = 0usize;
    for hash in residual_by_hash.keys() {
        if !send_traces.iter().any(|trace| &trace.payment_hash == hash) {
            residual_without_trace += 1;
            debug!(
                "Residual payment_hash {:?} not found in pre-restart trace list",
                hash
            );
        }
    }
    for trace in &send_traces {
        if !residual_by_hash.contains_key(&trace.payment_hash) {
            traces_with_no_residual += 1;
            debug!(
                "Trace payment_hash={} sender={} seq={} path={} disappeared after restart (no residual tlc entries)",
                trace.payment_hash,
                trace.sender,
                trace.seq,
                trace.route_channel_outpoints.join(" -> ")
            );
        }
    }
    debug!(
        "Reconcile summary: pre_restart_traces={} residual_unique_hash={} residual_total_tlcs={} missing_trace_no_residual={} residual_without_trace={}",
        send_traces.len(),
        residual_by_hash.len(),
        residual_count,
        traces_with_no_residual,
        residual_without_trace
    );
    let has_stuck_tlcs = !stuck_channels.is_empty();
    if has_stuck_tlcs {
        error!("Stuck TLCs detected after restart: {:?}", stuck_channels);
    }

    let final_total = node_a.get_local_balance_from_channel(channels[0])
        + node_a.get_local_balance_from_channel(channels[3])
        + node_b.get_local_balance_from_channel(channels[0])
        + node_b.get_local_balance_from_channel(channels[1])
        + node_c.get_local_balance_from_channel(channels[1])
        + node_c.get_local_balance_from_channel(channels[2])
        + node_d.get_local_balance_from_channel(channels[2])
        + node_d.get_local_balance_from_channel(channels[3]);
    assert_eq!(
        initial_total, final_total,
        "Total balance across all channels should be conserved.\n  initial: {}\n  final:   {}",
        initial_total, final_total
    );

    let mut missing_session_count = 0usize;
    for trace in &send_traces {
        let session = source_nodes[trace.sender_idx].get_payment_session(trace.payment_hash);
        if session.is_none() {
            missing_session_count += 1;
            debug!(
                "Trace payment_hash={} sender={} seq={} path={} has no post-restart session",
                trace.payment_hash,
                trace.sender,
                trace.seq,
                trace.route_channel_outpoints.join(" -> ")
            );
            if residual_by_hash.contains_key(&trace.payment_hash) {
                debug!(
                    "  missing session but still residual exists: {:?}",
                    trace_by_hash
                        .get(&trace.payment_hash)
                        .map(|t| t.route_channel_outpoints.join(" -> "))
                        .unwrap_or_default()
                );
            }
            continue;
        }
        let session = session.expect("checked");
        let mut has_inflight = false;
        for attempt in session.attempts() {
            if matches!(
                attempt.status,
                AttemptStatus::Inflight | AttemptStatus::Retrying
            ) {
                has_inflight = true;
            }
        }
        let has_unknown_next_peer = session.last_error_code == Some(TlcErrorCode::UnknownNextPeer);
        debug!(
            "post-restart session hash={} sender={} seq={} status={:?} attempts_inflight={} unknown_next_peer={} last_err={:?} residual_seen={}",
            trace.payment_hash,
            trace.sender,
            trace.seq,
            session.status,
            has_inflight,
            has_unknown_next_peer,
            session.last_error,
            residual_by_hash.contains_key(&trace.payment_hash)
        );
        if residual_by_hash.contains_key(&trace.payment_hash) && has_unknown_next_peer {
            let residual_nodes = residual_by_hash
                .get(&trace.payment_hash)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|(node, ch, id, status)| {
                            format!(
                                "node={} ch={:?} tlc_id={:?} status={:?}",
                                node, ch, id, status
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_else(|| "no residual location".to_string());
            debug!(
                "trace_with_unknown_next_peer hash={} path={} residual_locations={}",
                trace.payment_hash,
                trace.route_channel_outpoints.join(" -> "),
                residual_nodes
            );
        }
    }
    debug!(
        "post-restart session summary: missing_session_count={}",
        missing_session_count
    );

    assert!(
        !has_stuck_tlcs,
        "Node still has stuck TLCs after restart: {:?}",
        stuck_channels
    );

    debug!("test_ring_self_payments_then_restart_two_nodes completed successfully");
}

/// Reproducer for disconnect/reconnect recovery on a topology that is known to
/// support self-MPP. It mixes direct MPP and invoice-based MPP self-payments,
/// disconnects the middle peers for 10 seconds, reconnects them, then verifies
/// all payments reach a final state and balances/TLCs fully reconcile.
#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
#[ignore] // Long-running reproducer kept separate from restart coverage.
async fn test_self_mpp_then_disconnect_and_reconnect_middle_peers() {
    init_tracing();

    #[derive(Debug, Clone)]
    struct PaymentTrace {
        kind: &'static str,
        payment_hash: Hash256,
    }

    async fn send_self_mpp_payment(
        node: &NetworkNode,
        amount: u128,
        max_parts: u64,
    ) -> Result<crate::fiber::network::SendPaymentResponse, String> {
        node.send_mpp_payment_with_command(
            node,
            amount,
            SendPaymentCommand {
                max_parts: Some(max_parts),
                allow_self_payment: true,
                ..Default::default()
            },
        )
        .await
    }

    async fn send_self_mpp_invoice_payment(
        node: &NetworkNode,
        amount: u128,
        description: String,
        max_parts: u64,
    ) -> Result<crate::fiber::network::SendPaymentResponse, String> {
        let invoice = node
            .gen_invoice(NewInvoiceParams {
                amount,
                description: Some(description),
                allow_mpp: Some(true),
                ..Default::default()
            })
            .await;
        node.send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address),
            allow_self_payment: true,
            max_parts: Some(max_parts),
            ..Default::default()
        })
        .await
    }

    fn assert_channels_clean(checks: &[(&str, &NetworkNode, Vec<Hash256>)]) {
        for (name, node, channels) in checks {
            for channel_id in channels {
                let state = node.get_channel_actor_state(*channel_id);
                let tlcs = state.tlc_state.all_tlcs().collect::<Vec<_>>();
                assert!(
                    tlcs.is_empty(),
                    "node {} channel {:?} still has residual TLCs: {:?}",
                    name,
                    channel_id,
                    tlcs
                );
                assert!(
                    !state.reestablishing,
                    "node {} channel {:?} is still reestablishing",
                    name, channel_id
                );
            }
        }
    }

    async fn wait_until_middle_channel_recovered(
        node_b: &NetworkNode,
        node_c: &NetworkNode,
        channel_id: Hash256,
    ) {
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(30);
        loop {
            let b_state = node_b.get_channel_actor_state(channel_id);
            let c_state = node_c.get_channel_actor_state(channel_id);
            if !b_state.reestablishing && !c_state.reestablishing {
                return;
            }
            if start.elapsed() >= timeout {
                panic!(
                    "timed out waiting for middle channel {:?} to recover after reconnect",
                    channel_id
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn wait_until_channels_clean(checks: &[(&str, &NetworkNode, Vec<Hash256>)]) {
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(60);
        loop {
            let mut stuck_channels = Vec::new();
            for (name, node, channel_ids) in checks {
                for channel_id in channel_ids {
                    let state = node.get_channel_actor_state(*channel_id);
                    let tlcs = state.tlc_state.all_tlcs().collect::<Vec<_>>();
                    if state.reestablishing || !tlcs.is_empty() {
                        stuck_channels.push(format!(
                            "node={} channel={:?} reestablishing={} tlcs={:?}",
                            name, channel_id, state.reestablishing, tlcs
                        ));
                    }
                }
            }
            let clean = stuck_channels.is_empty();
            if clean {
                return;
            }
            if start.elapsed() >= timeout {
                panic!(
                    "timed out waiting for all channels to become clean: {:?}",
                    stuck_channels
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    let payment_amount = 30_000u128;
    let mpp_max_parts = 3u64;

    let (nodes, channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters::new(MIN_RESERVED_CKB + 66_000, HUGE_CKB_AMOUNT),
            ), // A-B
            (
                (1, 2),
                ChannelParameters::new(HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT),
            ), // B-C
            (
                (2, 3),
                ChannelParameters::new(MIN_RESERVED_CKB + 11_000, MIN_RESERVED_CKB),
            ), // C-D #1
            (
                (2, 3),
                ChannelParameters::new(MIN_RESERVED_CKB + 11_000, MIN_RESERVED_CKB),
            ), // C-D #2
            (
                (2, 3),
                ChannelParameters::new(MIN_RESERVED_CKB + 11_000, MIN_RESERVED_CKB),
            ), // C-D #3
            (
                (3, 0),
                ChannelParameters::new(MIN_RESERVED_CKB + 76_000, HUGE_CKB_AMOUNT),
            ), // D-A
        ],
        4,
        Some(gen_rpc_config()),
    )
    .await;
    let [node_a, node_b, node_c, node_d] = nodes.try_into().expect("4 nodes");
    let source_node = &node_a;
    let node_checks = vec![
        ("A", &node_a, vec![channels[0], channels[5]]),
        ("B", &node_b, vec![channels[0], channels[1]]),
        (
            "C",
            &node_c,
            vec![channels[1], channels[2], channels[3], channels[4]],
        ),
        (
            "D",
            &node_d,
            vec![channels[2], channels[3], channels[4], channels[5]],
        ),
    ];
    let channel_checks = vec![
        (channels[0], &node_a, &node_b),
        (channels[1], &node_b, &node_c),
        (channels[2], &node_c, &node_d),
        (channels[3], &node_c, &node_d),
        (channels[4], &node_c, &node_d),
        (channels[5], &node_d, &node_a),
    ];
    let expected_channel_totals = channel_checks
        .iter()
        .map(|(channel_id, left, right)| {
            (
                *channel_id,
                left.get_local_balance_from_channel(*channel_id)
                    + right.get_local_balance_from_channel(*channel_id),
            )
        })
        .collect::<HashMap<_, _>>();
    let initial_total = expected_channel_totals.values().copied().sum::<u128>();
    let mut traffic_traces = Vec::new();

    let panic_events = vec!["panic".to_string(), "panicked".to_string()];
    node_a.add_unexpected_events(panic_events.clone()).await;
    node_b.add_unexpected_events(panic_events.clone()).await;
    node_c.add_unexpected_events(panic_events.clone()).await;
    node_d.add_unexpected_events(panic_events.clone()).await;

    assert_channels_clean(&node_checks);

    debug!("=== Sending pre-disconnect self-MPP traffic from node A ===");
    let direct_payment = send_self_mpp_payment(source_node, payment_amount, mpp_max_parts)
        .await
        .expect("pre-disconnect direct self-MPP payment");
    assert_eq!(
        direct_payment.routers.len(),
        3,
        "pre-disconnect direct self-MPP should use 3 parts"
    );
    traffic_traces.push(PaymentTrace {
        kind: "mpp",
        payment_hash: direct_payment.payment_hash,
    });

    let invoice_payment = send_self_mpp_invoice_payment(
        source_node,
        payment_amount,
        "pre-disconnect-invoice".to_string(),
        mpp_max_parts,
    )
    .await
    .expect("pre-disconnect invoice self-MPP payment");
    assert_eq!(
        invoice_payment.routers.len(),
        3,
        "pre-disconnect invoice self-MPP should use 3 parts"
    );
    traffic_traces.push(PaymentTrace {
        kind: "mpp-invoice",
        payment_hash: invoice_payment.payment_hash,
    });

    debug!("=== Disconnecting node B from node C ===");
    node_b
        .send_rpc_request::<_, ()>(
            "disconnect_peer",
            DisconnectPeerParams {
                pubkey: node_c.pubkey.into(),
            },
        )
        .await
        .expect("disconnect peer B->C");

    tokio::time::sleep(Duration::from_secs(10)).await;

    debug!("=== Reconnecting node B to node C ===");
    node_b
        .send_rpc_request::<_, ()>(
            "connect_peer",
            ConnectPeerParams {
                address: None,
                pubkey: Some(node_c.pubkey.into()),
                save: Some(false),
            },
        )
        .await
        .expect("reconnect peer B->C");
    wait_until_middle_channel_recovered(&node_b, &node_c, channels[1]).await;

    for trace in &traffic_traces {
        source_node
            .wait_until_final_status(trace.payment_hash)
            .await;
        let status = source_node.get_payment_status(trace.payment_hash).await;
        debug!(
            "disconnect-window trace kind={} hash={:?} final_status={:?}",
            trace.kind, trace.payment_hash, status
        );
    }

    debug!("=== Sending post-settlement recovery payments ===");
    let recovery_direct = send_self_mpp_payment(source_node, payment_amount, mpp_max_parts)
        .await
        .expect("post-settlement direct self-MPP payment");
    source_node
        .wait_until_success(recovery_direct.payment_hash)
        .await;
    let recovery_direct_result = source_node
        .get_payment_result(recovery_direct.payment_hash)
        .await;
    assert_eq!(
        recovery_direct_result.routers.len(),
        3,
        "recovery direct self-payment did not actually use MPP"
    );
    debug!(
        "post-settlement recovery trace kind=mpp hash={:?} status={:?} parts={}",
        recovery_direct.payment_hash,
        recovery_direct_result.status,
        recovery_direct_result.routers.len()
    );

    let recovery_invoice = send_self_mpp_invoice_payment(
        source_node,
        payment_amount,
        "after-settlement-invoice".to_string(),
        mpp_max_parts,
    )
    .await
    .expect("post-settlement invoice self-MPP payment");
    source_node
        .wait_until_success(recovery_invoice.payment_hash)
        .await;
    let recovery_invoice_result = source_node
        .get_payment_result(recovery_invoice.payment_hash)
        .await;
    assert_eq!(
        recovery_invoice_result.routers.len(),
        3,
        "recovery invoice self-payment did not actually use MPP"
    );
    debug!(
        "post-settlement recovery trace kind=mpp-invoice hash={:?} status={:?} parts={}",
        recovery_invoice.payment_hash,
        recovery_invoice_result.status,
        recovery_invoice_result.routers.len()
    );

    wait_until_channels_clean(&node_checks).await;

    for (name, node) in [
        ("A", &node_a),
        ("B", &node_b),
        ("C", &node_c),
        ("D", &node_d),
    ] {
        let events = node.get_triggered_unexpected_events().await;
        assert!(
            events.is_empty(),
            "node {} got unexpected events: {:?}",
            name,
            events
        );
    }

    let final_total = channel_checks
        .iter()
        .map(|(channel_id, left, right)| {
            left.get_local_balance_from_channel(*channel_id)
                + right.get_local_balance_from_channel(*channel_id)
        })
        .sum::<u128>();
    assert_eq!(
        initial_total, final_total,
        "Total balance across all channels should be conserved.\n  initial: {}\n  final:   {}",
        initial_total, final_total
    );

    for (channel_id, left, right) in &channel_checks {
        let final_channel_total = left.get_local_balance_from_channel(*channel_id)
            + right.get_local_balance_from_channel(*channel_id);
        let expected_channel_total = expected_channel_totals
            .get(channel_id)
            .copied()
            .expect("expected channel total");
        assert_eq!(
            final_channel_total, expected_channel_total,
            "channel {:?} total balance drifted: expected {}, got {}",
            channel_id, expected_channel_total, final_channel_total
        );
    }

    assert_channels_clean(&node_checks);

    debug!("test_self_mpp_then_disconnect_and_reconnect_middle_peers completed successfully");
}

/// Diagnostic variant of the disconnect/reconnect reproducer. After the same
/// disconnect window, it bypasses automatic route selection and sends explicit
/// single-path self-payments over each C-D channel. If these succeed while the
/// automatic self-MPP reproducer fails, the fault is more likely in payment
/// path selection/history than in basic channel recovery.
#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
#[ignore]
async fn test_self_mpp_disconnect_reconnect_explicit_single_routes_still_work() {
    init_tracing();

    async fn send_self_mpp_payment(
        node: &NetworkNode,
        amount: u128,
        max_parts: u64,
    ) -> Result<crate::fiber::network::SendPaymentResponse, String> {
        node.send_mpp_payment_with_command(
            node,
            amount,
            SendPaymentCommand {
                max_parts: Some(max_parts),
                allow_self_payment: true,
                ..Default::default()
            },
        )
        .await
    }

    async fn send_self_mpp_invoice_payment(
        node: &NetworkNode,
        amount: u128,
        description: String,
        max_parts: u64,
    ) -> Result<crate::fiber::network::SendPaymentResponse, String> {
        let invoice = node
            .gen_invoice(NewInvoiceParams {
                amount,
                description: Some(description),
                allow_mpp: Some(true),
                ..Default::default()
            })
            .await;
        node.send_payment(SendPaymentCommand {
            invoice: Some(invoice.invoice_address),
            allow_self_payment: true,
            max_parts: Some(max_parts),
            ..Default::default()
        })
        .await
    }

    fn assert_channels_clean(checks: &[(&str, &NetworkNode, Vec<Hash256>)]) {
        for (name, node, channels) in checks {
            for channel_id in channels {
                let state = node.get_channel_actor_state(*channel_id);
                let tlcs = state.tlc_state.all_tlcs().collect::<Vec<_>>();
                assert!(
                    tlcs.is_empty(),
                    "node {} channel {:?} still has residual TLCs: {:?}",
                    name,
                    channel_id,
                    tlcs
                );
                assert!(
                    !state.reestablishing,
                    "node {} channel {:?} is still reestablishing",
                    name, channel_id
                );
            }
        }
    }

    async fn wait_until_middle_channel_recovered(
        node_b: &NetworkNode,
        node_c: &NetworkNode,
        channel_id: Hash256,
    ) {
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(30);
        loop {
            let b_state = node_b.get_channel_actor_state(channel_id);
            let c_state = node_c.get_channel_actor_state(channel_id);
            if !b_state.reestablishing && !c_state.reestablishing {
                return;
            }
            if start.elapsed() >= timeout {
                panic!(
                    "timed out waiting for middle channel {:?} to recover after reconnect",
                    channel_id
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn wait_until_channels_clean(checks: &[(&str, &NetworkNode, Vec<Hash256>)]) {
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(60);
        loop {
            let mut stuck_channels = Vec::new();
            for (name, node, channel_ids) in checks {
                for channel_id in channel_ids {
                    let state = node.get_channel_actor_state(*channel_id);
                    let tlcs = state.tlc_state.all_tlcs().collect::<Vec<_>>();
                    if state.reestablishing || !tlcs.is_empty() {
                        stuck_channels.push(format!(
                            "node={} channel={:?} reestablishing={} tlcs={:?}",
                            name, channel_id, state.reestablishing, tlcs
                        ));
                    }
                }
            }
            if stuck_channels.is_empty() {
                return;
            }
            if start.elapsed() >= timeout {
                panic!(
                    "timed out waiting for all channels to become clean: {:?}",
                    stuck_channels
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn build_explicit_self_route(
        source_node: &NetworkNode,
        node_b: &NetworkNode,
        node_c: &NetworkNode,
        node_d: &NetworkNode,
        amount: u128,
        ab_outpoint: ckb_types::packed::OutPoint,
        bc_outpoint: ckb_types::packed::OutPoint,
        cd_outpoint: ckb_types::packed::OutPoint,
        da_outpoint: ckb_types::packed::OutPoint,
    ) -> crate::fiber::network::PaymentRouter {
        source_node
            .build_router(crate::fiber::network::BuildRouterCommand {
                amount: Some(amount),
                udt_type_script: None,
                final_tlc_expiry_delta: None,
                hops_info: vec![
                    fiber_types::HopRequire {
                        pubkey: node_b.pubkey,
                        channel_outpoint: Some(ab_outpoint),
                    },
                    fiber_types::HopRequire {
                        pubkey: node_c.pubkey,
                        channel_outpoint: Some(bc_outpoint),
                    },
                    fiber_types::HopRequire {
                        pubkey: node_d.pubkey,
                        channel_outpoint: Some(cd_outpoint),
                    },
                    fiber_types::HopRequire {
                        pubkey: source_node.pubkey,
                        channel_outpoint: Some(da_outpoint),
                    },
                ],
            })
            .await
            .expect("build explicit self route")
    }

    let pre_disconnect_amount = 30_000u128;
    let explicit_route_amount = 9_000u128;
    let mpp_max_parts = 3u64;

    let (nodes, channels) = create_n_nodes_network_with_params(
        &[
            (
                (0, 1),
                ChannelParameters::new(MIN_RESERVED_CKB + 66_000, HUGE_CKB_AMOUNT),
            ), // A-B
            (
                (1, 2),
                ChannelParameters::new(HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT),
            ), // B-C
            (
                (2, 3),
                ChannelParameters::new(MIN_RESERVED_CKB + 11_000, MIN_RESERVED_CKB),
            ), // C-D #1
            (
                (2, 3),
                ChannelParameters::new(MIN_RESERVED_CKB + 11_000, MIN_RESERVED_CKB),
            ), // C-D #2
            (
                (2, 3),
                ChannelParameters::new(MIN_RESERVED_CKB + 11_000, MIN_RESERVED_CKB),
            ), // C-D #3
            (
                (3, 0),
                ChannelParameters::new(MIN_RESERVED_CKB + 76_000, HUGE_CKB_AMOUNT),
            ), // D-A
        ],
        4,
        Some(gen_rpc_config()),
    )
    .await;
    let [node_a, node_b, node_c, node_d] = nodes.try_into().expect("4 nodes");
    let source_node = &node_a;
    let node_checks = vec![
        ("A", &node_a, vec![channels[0], channels[5]]),
        ("B", &node_b, vec![channels[0], channels[1]]),
        (
            "C",
            &node_c,
            vec![channels[1], channels[2], channels[3], channels[4]],
        ),
        (
            "D",
            &node_d,
            vec![channels[2], channels[3], channels[4], channels[5]],
        ),
    ];
    let channel_checks = vec![
        (channels[0], &node_a, &node_b),
        (channels[1], &node_b, &node_c),
        (channels[2], &node_c, &node_d),
        (channels[3], &node_c, &node_d),
        (channels[4], &node_c, &node_d),
        (channels[5], &node_d, &node_a),
    ];
    let expected_channel_totals = channel_checks
        .iter()
        .map(|(channel_id, left, right)| {
            (
                *channel_id,
                left.get_local_balance_from_channel(*channel_id)
                    + right.get_local_balance_from_channel(*channel_id),
            )
        })
        .collect::<HashMap<_, _>>();
    let initial_total = expected_channel_totals.values().copied().sum::<u128>();

    let panic_events = vec!["panic".to_string(), "panicked".to_string()];
    node_a.add_unexpected_events(panic_events.clone()).await;
    node_b.add_unexpected_events(panic_events.clone()).await;
    node_c.add_unexpected_events(panic_events.clone()).await;
    node_d.add_unexpected_events(panic_events.clone()).await;

    assert_channels_clean(&node_checks);

    let direct_payment = send_self_mpp_payment(source_node, pre_disconnect_amount, mpp_max_parts)
        .await
        .expect("pre-disconnect direct self-MPP payment");
    assert_eq!(direct_payment.routers.len(), 3);

    let invoice_payment = send_self_mpp_invoice_payment(
        source_node,
        pre_disconnect_amount,
        "pre-disconnect-invoice".to_string(),
        mpp_max_parts,
    )
    .await
    .expect("pre-disconnect invoice self-MPP payment");
    assert_eq!(invoice_payment.routers.len(), 3);

    node_b
        .send_rpc_request::<_, ()>(
            "disconnect_peer",
            DisconnectPeerParams {
                pubkey: node_c.pubkey.into(),
            },
        )
        .await
        .expect("disconnect peer B->C");

    tokio::time::sleep(Duration::from_secs(10)).await;

    node_b
        .send_rpc_request::<_, ()>(
            "connect_peer",
            ConnectPeerParams {
                address: None,
                pubkey: Some(node_c.pubkey.into()),
                save: Some(false),
            },
        )
        .await
        .expect("reconnect peer B->C");
    wait_until_middle_channel_recovered(&node_b, &node_c, channels[1]).await;
    // Reestablish flips `reestablishing=false` before the delayed ChannelReady
    // event repopulates `network.outpoint_channel_map`.
    tokio::time::sleep(Duration::from_secs(5)).await;

    for payment_hash in [direct_payment.payment_hash, invoice_payment.payment_hash] {
        source_node.wait_until_final_status(payment_hash).await;
        debug!(
            "explicit-route diagnostic preexisting hash={:?} final_status={:?}",
            payment_hash,
            source_node.get_payment_status(payment_hash).await
        );
    }

    let ab_outpoint = node_a
        .get_channel_outpoint(&channels[0])
        .expect("A-B outpoint");
    let bc_outpoint = node_b
        .get_channel_outpoint(&channels[1])
        .expect("B-C outpoint");
    let da_outpoint = node_d
        .get_channel_outpoint(&channels[5])
        .expect("D-A outpoint");

    for (name, cd_channel_id) in [
        ("C-D#1", channels[2]),
        ("C-D#2", channels[3]),
        ("C-D#3", channels[4]),
    ] {
        let cd_outpoint = node_c
            .get_channel_outpoint(&cd_channel_id)
            .expect("C-D outpoint");
        let router = build_explicit_self_route(
            source_node,
            &node_b,
            &node_c,
            &node_d,
            explicit_route_amount,
            ab_outpoint.clone(),
            bc_outpoint.clone(),
            cd_outpoint,
            da_outpoint.clone(),
        )
        .await;
        assert_eq!(
            router.router_hops.len(),
            4,
            "explicit self route over {} should contain 4 hops",
            name
        );
        let payment = source_node
            .send_payment_with_router(SendPaymentWithRouterCommand {
                router: router.router_hops.clone(),
                keysend: Some(true),
                ..Default::default()
            })
            .await
            .expect("explicit self route payment");
        source_node.wait_until_success(payment.payment_hash).await;
        debug!(
            "explicit-route diagnostic {} payment_hash={:?} fee={}",
            name, payment.payment_hash, payment.fee
        );
    }

    wait_until_channels_clean(&node_checks).await;

    for (name, node) in [
        ("A", &node_a),
        ("B", &node_b),
        ("C", &node_c),
        ("D", &node_d),
    ] {
        let events = node.get_triggered_unexpected_events().await;
        assert!(
            events.is_empty(),
            "node {} got unexpected events: {:?}",
            name,
            events
        );
    }

    let final_total = channel_checks
        .iter()
        .map(|(channel_id, left, right)| {
            left.get_local_balance_from_channel(*channel_id)
                + right.get_local_balance_from_channel(*channel_id)
        })
        .sum::<u128>();
    assert_eq!(initial_total, final_total);

    for (channel_id, left, right) in &channel_checks {
        let final_channel_total = left.get_local_balance_from_channel(*channel_id)
            + right.get_local_balance_from_channel(*channel_id);
        let expected_channel_total = expected_channel_totals
            .get(channel_id)
            .copied()
            .expect("expected channel total");
        assert_eq!(final_channel_total, expected_channel_total);
    }

    assert_channels_clean(&node_checks);

    debug!("test_self_mpp_disconnect_reconnect_explicit_single_routes_still_work completed successfully");
}
