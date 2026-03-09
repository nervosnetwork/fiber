use crate::fiber::types::Hash256;
use crate::test_utils::{init_tracing, NetworkNode};
use crate::tests::test_utils::{
    create_n_nodes_network, create_nodes_with_established_channel, HUGE_CKB_AMOUNT,
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
