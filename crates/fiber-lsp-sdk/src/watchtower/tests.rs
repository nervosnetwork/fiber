use super::*;

#[derive(Default)]
struct RecordingHandler {
    quotas: Vec<(WatchtowerQuotaError, WatchtowerQuotaAction)>,
    statuses: Vec<WatchtowerHealthStatus>,
}

impl WatchtowerEventHandler for RecordingHandler {
    fn on_quota_exceeded(
        &mut self,
        error: &WatchtowerQuotaError,
        recommended_action: WatchtowerQuotaAction,
    ) {
        self.quotas.push((error.clone(), recommended_action));
    }

    fn on_status_update(&mut self, status: &WatchtowerHealthStatus) {
        self.statuses.push(status.clone());
    }
}

#[test]
fn quota_error_parsing_and_policy_decisions() {
    let cases = [
        (
            "watchtower channel quota exceeded for node (max 64)",
            WatchtowerQuotaKind::Channel,
            WatchtowerQuotaScope::Tenant,
            WatchtowerQuotaAction::FailClosed,
            WatchtowerQuotaAction::PruneAndRetry,
        ),
        (
            "watchtower global channel quota exceeded (max 4096)",
            WatchtowerQuotaKind::Channel,
            WatchtowerQuotaScope::Global,
            WatchtowerQuotaAction::FailClosed,
            WatchtowerQuotaAction::FailClosed,
        ),
        (
            "watchtower preimage quota exceeded for node (max 256)",
            WatchtowerQuotaKind::Preimage,
            WatchtowerQuotaScope::Tenant,
            WatchtowerQuotaAction::FailClosed,
            WatchtowerQuotaAction::PruneAndRetry,
        ),
        (
            "watchtower global preimage quota exceeded (max 16384)",
            WatchtowerQuotaKind::Preimage,
            WatchtowerQuotaScope::Global,
            WatchtowerQuotaAction::FailClosed,
            WatchtowerQuotaAction::FailClosed,
        ),
    ];

    for (msg, kind, scope, fail_closed_action, prune_action) in cases {
        let parsed = WatchtowerQuotaError::parse_rpc_error(msg).expect("parsed quota error");
        assert_eq!(parsed.kind, kind);
        assert_eq!(parsed.scope, scope);
        assert_eq!(
            WatchtowerLimitPolicy::FailClosed.decide(&parsed),
            fail_closed_action
        );
        assert_eq!(
            WatchtowerLimitPolicy::PruneAndRetry.decide(&parsed),
            prune_action
        );

        let mut handler = RecordingHandler::default();
        let decided =
            handle_watchtower_rpc_error(WatchtowerLimitPolicy::PruneAndRetry, &mut handler, msg);
        assert_eq!(decided, Some(prune_action));
        assert_eq!(handler.quotas.len(), 1);
        assert_eq!(handler.quotas[0].1, prune_action);
    }

    assert!(WatchtowerQuotaError::parse_rpc_error("watched channel not found").is_none());
}

#[cfg(feature = "json")]
#[test]
fn evaluate_watchtower_status_reports_healthy_pending_and_backoff() {
    let channel_id = Hash256::from([1; 32]);
    let request_id = Hash256::from([2; 32]);

    let backoff_rpc = fiber_json_types::GetWatchtowerSigningStatusResult {
        channel_id: channel_id.into(),
        status: fiber_json_types::WatchtowerSigningStatus::Backoff {
            retry_after_ms: 1_000,
            consecutive_expired_requests: 1,
        },
    };
    // Once now_ms >= retry_after_ms, backoff has elapsed -> Healthy.
    assert_eq!(
        evaluate_watchtower_status(&backoff_rpc, 1_000),
        WatchtowerHealthStatus::Healthy { channel_id }
    );

    // Before retry_after_ms -> InBackoff.
    let backoff = evaluate_watchtower_status(&backoff_rpc, 400);
    assert_eq!(
        backoff,
        WatchtowerHealthStatus::InBackoff {
            channel_id,
            retry_after_ms: 1_000,
            remaining_backoff_ms: 600,
            consecutive_expired_requests: 1,
        }
    );
    // Dispute deadline is 1_500 with 600ms safety margin (cutoff = 900 <= retry_after_ms 1_000).
    assert!(backoff.needs_self_broadcast(1_500, 400, 600));
    // Dispute deadline far in the future -> no self-broadcast needed yet.
    assert!(!backoff.needs_self_broadcast(10_000, 400, 600));

    // ExternalFundingRequired maps to PendingExternalFunding with TTL and optional required_expiry_since.
    let external_funding_rpc = fiber_json_types::GetWatchtowerSigningStatusResult {
        channel_id: channel_id.into(),
        status: fiber_json_types::WatchtowerSigningStatus::ExternalFundingRequired {
            request_id: request_id.into(),
            content: fiber_json_types::OnchainSigningContent {
                key_purpose: fiber_json_types::OnchainKeyPurpose::Tlc {
                    commitment_number: 3,
                },
                transaction: ckb_types::core::TransactionBuilder::default()
                    .build()
                    .data()
                    .into(),
            },
            expires_at_ms: 3_000,
            required_expiry_since: Some(0x4000_0000_0000_0064),
        },
    };
    let ext_funding = evaluate_watchtower_status(&external_funding_rpc, 1_000);
    assert_eq!(
        ext_funding,
        WatchtowerHealthStatus::PendingExternalFunding {
            channel_id,
            request_id,
            expires_at_ms: 3_000,
            remaining_ms: 2_000,
            required_expiry_since: Some(0x4000_0000_0000_0064),
        }
    );
    assert!(!ext_funding.needs_self_broadcast(3_500, 1_000, 500));
    assert!(ext_funding.needs_self_broadcast(3_500, 3_000, 500));

    // Expired pending request near dispute deadline -> trigger self-broadcast.
    let expired_pending = evaluate_watchtower_status(&external_funding_rpc, 3_100);
    assert!(expired_pending.needs_self_broadcast(3_500, 3_100, 500));

    // Even if an untrusted LSP reports Healthy or a future-TTL PendingExternalFunding,
    // reaching the dispute safety cutoff MUST still trigger self-broadcast.
    let healthy = WatchtowerHealthStatus::Healthy { channel_id };
    assert!(!healthy.needs_self_broadcast(2_500, 1_200, 500));
    assert!(healthy.needs_self_broadcast(2_500, 2_000, 500));
    assert!(ext_funding.needs_self_broadcast(2_500, 2_000, 500));
}
