use crate::fiber::network::*;
use crate::fiber::{PaymentStatus, Pubkey};
use crate::tests::test_utils::*;
use crate::NetworkServiceEvent;

use ractor::ActorRef;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct DirectKeysendTpsMeasurement {
    attempted: usize,
    send_errors: usize,
    succeeded: usize,
    failed: usize,
    measurement_window: Duration,
    avg_cpu_percent: Option<f64>,
    peak_cpu_percent: Option<f64>,
    cpu_samples: usize,
}

impl DirectKeysendTpsMeasurement {
    fn success_tps(&self) -> f64 {
        self.succeeded as f64 / self.measurement_window.as_secs_f64()
    }
}

async fn measure_direct_keysend_tps(
    sender: &NetworkNode,
    receiver: &NetworkNode,
    measurement_window: Duration,
    payment_amount: u128,
    max_inflight: usize,
) -> DirectKeysendTpsMeasurement {
    assert!(max_inflight > 0);

    let send_deadline = Instant::now() + measurement_window;
    let settle_deadline = send_deadline + Duration::from_secs(30);
    let mut measurement = DirectKeysendTpsMeasurement {
        attempted: 0,
        send_errors: 0,
        succeeded: 0,
        failed: 0,
        measurement_window,
        avg_cpu_percent: None,
        peak_cpu_percent: None,
        cpu_samples: 0,
    };
    let mut pending = Vec::new();
    let mut next_cpu_sample_at = Instant::now();
    let mut cpu_percent_sum = 0.0_f64;
    let mut cpu_percent_peak = 0.0_f64;

    loop {
        let now = Instant::now();
        let can_send_more = now < send_deadline;
        if !can_send_more && pending.is_empty() {
            break;
        }
        assert!(
            now < settle_deadline,
            "Timed out waiting for pending direct A->B payments to finish"
        );

        if now >= next_cpu_sample_at && now < send_deadline {
            if let Some(cpu_percent) = sample_current_process_cpu_percent() {
                cpu_percent_sum += cpu_percent;
                cpu_percent_peak = cpu_percent_peak.max(cpu_percent);
                measurement.cpu_samples += 1;
            }
            next_cpu_sample_at = now + Duration::from_secs(1);
        }

        while Instant::now() < send_deadline && pending.len() < max_inflight {
            match sender
                .send_payment_keysend(receiver, payment_amount, false)
                .await
            {
                Ok(payment) => {
                    measurement.attempted += 1;
                    pending.push(payment.payment_hash);
                }
                Err(err) => {
                    measurement.send_errors += 1;
                    eprintln!("send_payment_keysend error during stress measurement: {err}");
                    break;
                }
            }
        }

        let mut still_pending = Vec::with_capacity(pending.len());
        for payment_hash in pending {
            match sender.get_payment_status(payment_hash).await {
                PaymentStatus::Success => measurement.succeeded += 1,
                PaymentStatus::Failed => measurement.failed += 1,
                _ => still_pending.push(payment_hash),
            }
        }
        pending = still_pending;

        if Instant::now() < send_deadline || !pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    if measurement.cpu_samples > 0 {
        measurement.avg_cpu_percent = Some(cpu_percent_sum / measurement.cpu_samples as f64);
        measurement.peak_cpu_percent = Some(cpu_percent_peak);
    }

    assert_eq!(sender.get_inflight_payment_count().await, 0);
    measurement
}

fn sample_current_process_cpu_percent() -> Option<f64> {
    let pid = std::process::id().to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "%cpu=", "-p", &pid])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout.trim().parse::<f64>().ok()
}

fn stress_measurement_window() -> Duration {
    const DEFAULT_MEASUREMENT_WINDOW_SECS: u64 = 60;
    let seconds = std::env::var("FNN_STRESS_MEASUREMENT_WINDOW_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MEASUREMENT_WINDOW_SECS);
    Duration::from_secs(seconds)
}

async fn exercise_peer_reconnects(
    mut node_c: NetworkNode,
    node_b_pubkey: Pubkey,
    node_b_network_actor: ActorRef<NetworkActorMessage>,
    duration: Duration,
) -> (NetworkNode, usize) {
    let reconnect_deadline = Instant::now() + duration;
    let node_c_pubkey = node_c.pubkey;
    let node_c_addr = node_c.listening_addrs[0].clone();
    let mut reconnect_cycles = 0;

    while Instant::now() < reconnect_deadline {
        node_b_network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::DisconnectPeer(
                    node_c_pubkey,
                    PeerDisconnectReason::Requested,
                    None,
                ),
            ))
            .expect("node_b alive");

        node_c
            .expect_event(|event| match event {
                NetworkServiceEvent::PeerDisConnected(pubkey, _) => pubkey == &node_b_pubkey,
                _ => false,
            })
            .await;

        tokio::time::sleep(Duration::from_millis(50)).await;

        node_b_network_actor
            .send_message(NetworkActorMessage::new_command(
                NetworkActorCommand::ConnectPeer(node_c_addr.clone(), false, None),
            ))
            .expect("node_b alive");

        node_c
            .expect_event(|event| match event {
                NetworkServiceEvent::PeerConnected(pubkey, _) => pubkey == &node_b_pubkey,
                _ => false,
            })
            .await;

        reconnect_cycles += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    (node_c, reconnect_cycles)
}

#[tokio::test]
#[ignore]
async fn test_stress_direct_tps_with_unrelated_peer_reconnects() {
    init_tracing();
    let _span = tracing::info_span!("node", node = "test").entered();

    const PAYMENT_AMOUNT: u128 = 1_000;
    const BC_CHANNEL_COUNT: usize = 24;
    const MAX_INFLIGHT: usize = 32;
    let measurement_window = stress_measurement_window();

    let mut channels = vec![((0, 1), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))];
    channels.extend((0..BC_CHANNEL_COUNT).map(|_| ((1, 2), (HUGE_CKB_AMOUNT, HUGE_CKB_AMOUNT))));

    let (nodes, _channels) = create_n_nodes_network(&channels, 3).await;
    let [node_a, node_b, node_c] = nodes.try_into().expect("3 nodes");

    let warmup = node_a
        .send_payment_keysend(&node_b, PAYMENT_AMOUNT, false)
        .await
        .unwrap();
    node_a.wait_until_success(warmup.payment_hash).await;

    let baseline = measure_direct_keysend_tps(
        &node_a,
        &node_b,
        measurement_window,
        PAYMENT_AMOUNT,
        MAX_INFLIGHT,
    )
    .await;

    let (with_reconnects, (node_c, reconnect_cycles)) = tokio::join!(
        measure_direct_keysend_tps(
            &node_a,
            &node_b,
            measurement_window,
            PAYMENT_AMOUNT,
            MAX_INFLIGHT,
        ),
        exercise_peer_reconnects(
            node_c,
            node_b.pubkey,
            node_b.network_actor.clone(),
            measurement_window
        ),
    );

    eprintln!("measurement_window_secs: {}", measurement_window.as_secs());
    eprintln!(
        "baseline: {:?}, success_tps={:.2}",
        baseline,
        baseline.success_tps()
    );
    eprintln!(
        "with_reconnects: {:?}, success_tps={:.2}",
        with_reconnects,
        with_reconnects.success_tps()
    );
    eprintln!("reconnect_cycles: {:?}", reconnect_cycles);

    assert!(baseline.succeeded > 0);
    assert_eq!(baseline.failed, 0);
    assert_eq!(baseline.send_errors, 0);
    assert!(with_reconnects.succeeded > 0);
    assert_eq!(with_reconnects.failed, 0);
    assert_eq!(with_reconnects.send_errors, 0);
    assert!(reconnect_cycles > 0);
    assert!(node_a.get_triggered_unexpected_events().await.is_empty());
    assert!(node_b.get_triggered_unexpected_events().await.is_empty());
    assert!(node_c.get_triggered_unexpected_events().await.is_empty());
}
