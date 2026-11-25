//! Watchtower quota classification, limit policies, status feedback, and handler callbacks.
//!
//! These helpers keep `fiber-lsp-sdk` transport- and runtime-agnostic while giving
//! wallet applications structured signals for watchtower channel/preimage quotas,
//! fee-hold TTLs, and per-node retry backoff.

use fiber_types::Hash256;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Which watchtower resource quota was hit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WatchtowerQuotaKind {
    /// Watched channel registration (`create_watch_channel`).
    Channel,
    /// Watched preimage registration (`create_preimage` / hosted `PreimageCreated`).
    Preimage,
}

/// Whether the quota limit applies to this tenant alone or to the entire LSP node.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WatchtowerQuotaScope {
    /// Per-tenant (`node_id`) cap; reclaimable by pruning closed channels or settled/expired preimages.
    Tenant,
    /// Global LSP operator cap; pruning local items alone cannot guarantee capacity.
    Global,
}

/// Structured classification of a watchtower quota RPC error.
#[derive(Clone, Debug, Deserialize, Eq, Error, PartialEq, Serialize)]
#[error("watchtower {scope:?} {kind:?} quota exceeded: {message}")]
pub struct WatchtowerQuotaError {
    /// Resource type whose quota was exceeded.
    pub kind: WatchtowerQuotaKind,
    /// Whether this tenant or the global watchtower cap was reached.
    pub scope: WatchtowerQuotaScope,
    /// Raw RPC error message returned by the LSP node.
    pub message: String,
}

impl WatchtowerQuotaError {
    /// Classify an RPC error message from `create_watch_channel` or `create_preimage`.
    ///
    /// Returns `None` if the error is not a watchtower quota error.
    pub fn parse_rpc_error(message: &str) -> Option<Self> {
        let lower = message.to_ascii_lowercase();
        if !lower.contains("quota exceeded") {
            return None;
        }
        let kind = if lower.contains("channel") {
            WatchtowerQuotaKind::Channel
        } else if lower.contains("preimage") {
            WatchtowerQuotaKind::Preimage
        } else {
            return None;
        };
        let scope = if lower.contains("global") {
            WatchtowerQuotaScope::Global
        } else {
            WatchtowerQuotaScope::Tenant
        };
        Some(Self {
            kind,
            scope,
            message: message.to_string(),
        })
    }
}

/// Policy controlling how the application reacts to watchtower quota errors.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum WatchtowerLimitPolicy {
    /// Always fail closed when a watchtower channel or preimage quota is exceeded.
    ///
    /// This is the recommended default so newly opened channels or invoices are
    /// never silently left without watchtower protection.
    #[default]
    FailClosed,
    /// For tenant-scoped quota errors, ask the caller to prune closed channels or
    /// settled/expired preimages and retry once. Global quota errors still fail closed.
    PruneAndRetry,
}

/// Concrete action recommended by [`WatchtowerLimitPolicy::decide`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WatchtowerQuotaAction {
    /// Abort the operation; do not proceed with an unprotected channel or invoice.
    FailClosed,
    /// Prune closed channels (`remove_watch_channel`) or settled/expired preimages
    /// (`remove_preimage`) and retry once; fail closed if the retry still fails.
    PruneAndRetry,
}

impl WatchtowerLimitPolicy {
    /// Decide the safe action for a classified [`WatchtowerQuotaError`].
    pub fn decide(self, error: &WatchtowerQuotaError) -> WatchtowerQuotaAction {
        match (self, error.scope) {
            (Self::PruneAndRetry, WatchtowerQuotaScope::Tenant) => {
                WatchtowerQuotaAction::PruneAndRetry
            }
            (Self::FailClosed, _) | (Self::PruneAndRetry, WatchtowerQuotaScope::Global) => {
                WatchtowerQuotaAction::FailClosed
            }
        }
    }
}

/// Actionable health projection of a watched channel's external signing status.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WatchtowerHealthStatus {
    /// No signature is currently pending and no retry backoff is active.
    Healthy {
        /// Watched channel identifier.
        channel_id: Hash256,
    },
    /// Watchtower issued an unfunded template transaction that the client's external wallet must
    /// fund (covering fees and any CKB shortfall) and sign before submitting `signed_transaction`.
    PendingExternalFunding {
        /// Watched channel identifier.
        channel_id: Hash256,
        /// Outstanding signature request identifier.
        request_id: Hash256,
        /// Unix millisecond timestamp when the request TTL expires.
        expires_at_ms: u64,
        /// Remaining milliseconds before the request expires (`0` if already elapsed).
        remaining_ms: u64,
        /// Optional absolute timestamp `since` required on a funding input for a timeout TLC unlock.
        required_expiry_since: Option<u64>,
    },
    /// No signature is currently pending, and new signing requests are paused due to backoff.
    InBackoff {
        /// Watched channel identifier.
        channel_id: Hash256,
        /// Effective Unix millisecond timestamp when backoff expires (channel or tenant-wide).
        retry_after_ms: u64,
        /// Remaining milliseconds until the watchtower can create a new signing request.
        remaining_backoff_ms: u64,
        /// Consecutive expired or failed-broadcast requests recorded on this channel.
        consecutive_expired_requests: u32,
    },
}

impl WatchtowerHealthStatus {
    /// Return the channel identifier associated with this status.
    pub fn channel_id(&self) -> Hash256 {
        match self {
            Self::Healthy { channel_id }
            | Self::PendingExternalFunding { channel_id, .. }
            | Self::InBackoff { channel_id, .. } => *channel_id,
        }
    }

    /// Return whether the client should fall back to self-broadcasting a settlement
    /// transaction from its local recovery record because the dispute safety cutoff
    /// has been reached (never trusting an LSP-reported `Healthy` or future-TTL status
    /// past the cutoff) or the watchtower's backoff extends into the safety cutoff.
    pub fn needs_self_broadcast(
        &self,
        dispute_deadline_ms: u64,
        now_ms: u64,
        safety_margin_ms: u64,
    ) -> bool {
        let safe_cutoff = dispute_deadline_ms.saturating_sub(safety_margin_ms);
        if now_ms >= safe_cutoff {
            return true;
        }
        match self {
            Self::Healthy { .. } | Self::PendingExternalFunding { .. } => false,
            Self::InBackoff { retry_after_ms, .. } => *retry_after_ms >= safe_cutoff,
        }
    }
}

/// Application callback interface for watchtower quota errors and health status feedback.
///
/// Implementations remain synchronous and side-effect controlled by the caller,
/// matching `fiber-lsp-sdk`'s runtime-agnostic design.
pub trait WatchtowerEventHandler {
    /// Invoked when a watchtower quota error is classified, alongside the action
    /// recommended by the active [`WatchtowerLimitPolicy`].
    fn on_quota_exceeded(
        &mut self,
        _error: &WatchtowerQuotaError,
        _recommended_action: WatchtowerQuotaAction,
    ) {
    }

    /// Invoked whenever a watchtower signing status snapshot is evaluated.
    fn on_status_update(&mut self, _status: &WatchtowerHealthStatus) {}
}

/// No-op [`WatchtowerEventHandler`] for callers that inspect return values directly.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopWatchtowerEventHandler;

impl WatchtowerEventHandler for NoopWatchtowerEventHandler {}

/// Classify a watchtower RPC error message, apply `policy`, and notify `handler`.
///
/// Returns `Some(action)` if the error was a watchtower quota error, or `None` otherwise.
pub fn handle_watchtower_rpc_error(
    policy: WatchtowerLimitPolicy,
    handler: &mut impl WatchtowerEventHandler,
    message: &str,
) -> Option<WatchtowerQuotaAction> {
    let error = WatchtowerQuotaError::parse_rpc_error(message)?;
    let action = policy.decide(&error);
    handler.on_quota_exceeded(&error, action);
    Some(action)
}

/// Evaluate a `get_watchtower_signing_status` (or `watchtower_signing` subscription)
/// payload into a structured [`WatchtowerHealthStatus`].
#[cfg(feature = "json")]
pub fn evaluate_watchtower_status(
    result: &fiber_json_types::GetWatchtowerSigningStatusResult,
    now_ms: u64,
) -> WatchtowerHealthStatus {
    let channel_id: Hash256 = result.channel_id.into();
    match &result.status {
        fiber_json_types::WatchtowerSigningStatus::ExternalFundingRequired {
            request_id,
            expires_at_ms,
            required_expiry_since,
            ..
        } => WatchtowerHealthStatus::PendingExternalFunding {
            channel_id,
            request_id: (*request_id).into(),
            expires_at_ms: *expires_at_ms,
            remaining_ms: expires_at_ms.saturating_sub(now_ms),
            required_expiry_since: *required_expiry_since,
        },
        fiber_json_types::WatchtowerSigningStatus::Backoff {
            retry_after_ms,
            consecutive_expired_requests,
        } => {
            if *retry_after_ms > now_ms {
                WatchtowerHealthStatus::InBackoff {
                    channel_id,
                    retry_after_ms: *retry_after_ms,
                    remaining_backoff_ms: retry_after_ms.saturating_sub(now_ms),
                    consecutive_expired_requests: *consecutive_expired_requests,
                }
            } else {
                WatchtowerHealthStatus::Healthy { channel_id }
            }
        }
        fiber_json_types::WatchtowerSigningStatus::NoSignatureRequired => {
            WatchtowerHealthStatus::Healthy { channel_id }
        }
    }
}

#[cfg(test)]
mod tests {
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
            let decided = handle_watchtower_rpc_error(
                WatchtowerLimitPolicy::PruneAndRetry,
                &mut handler,
                msg,
            );
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
}
