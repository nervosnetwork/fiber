//! The hosted-delivery port the Fiber core talks to.
//!
//! The core has to ask one question about inbound trampoline payments ("does a
//! hosted tenant own this invoice?") and report facts back ("this payment is
//! final", "this tenant channel went online"). It must not know that the answer
//! comes from the LSP service, or that the LSP service exists at all: a node
//! without hosted tenants runs the same core.
//!
//! So the vocabulary and the trait live here, in the core, and
//! [`crate::lsp::LspDeliveryPort`] implements it by forwarding to the LSP
//! service actor. `crate::fiber` therefore never names `crate::lsp`; see the
//! `core_does_not_depend_on_the_lsp_module` test below.

use ckb_types::packed::Script;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::fiber::payment::{SendPaymentData, SendPaymentDataBuilder};
use crate::fiber_types::{
    EntityHex, Hash256, HashAlgorithm, PrevTlcInfo, Pubkey, TlcErrorCode, TrampolineContext,
};

/// Failure returned while starting a buffered hosted payment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryDispatchError {
    /// The runtime may become able to dispatch the payment before its deadline.
    Temporary { reason: String },
    /// Retrying the same request cannot succeed and the upstream TLC must fail.
    Permanent {
        reason: String,
        error_code: TlcErrorCode,
    },
}

impl std::fmt::Display for DeliveryDispatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Temporary { reason } | Self::Permanent { reason, .. } => {
                formatter.write_str(reason)
            }
        }
    }
}

impl std::error::Error for DeliveryDispatchError {}

#[cfg(not(target_arch = "wasm32"))]
/// Result of consulting the hosted invoice registry for an incoming trampoline TLC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryDecision {
    /// No hosted tenant owns this payment; the core forwards it itself.
    NotHosted,
    /// A hosted tenant owns it and buffered the TLC until it is ready.
    Buffered,
}

#[cfg(not(target_arch = "wasm32"))]
/// Action the core should take after the delivery service persisted an outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentOutcomeDecision {
    /// Resolve the upstream TLC with the downstream payment outcome.
    SettleUpstream,
    /// Keep the upstream TLC pending and retry delivery before its deadline.
    RetryDelivery,
}

#[cfg(not(target_arch = "wasm32"))]
/// A final downstream payment outcome reported to the delivery service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentOutcomeRequest {
    pub session_key: crate::fiber_types::PaymentSessionKey,
    pub payment_status: crate::fiber_types::PaymentStatus,
    pub failure: Option<String>,
    pub failure_code: Option<TlcErrorCode>,
}

#[cfg(not(target_arch = "wasm32"))]
/// Notification that the upstream TLC was settled with this outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettledPaymentOutcome {
    pub session_key: crate::fiber_types::PaymentSessionKey,
    pub payment_status: crate::fiber_types::PaymentStatus,
    pub failure: Option<String>,
}

/// A validated trampoline forwarding request that may be handed to a hosted
/// delivery service.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrampolineForwardingRequest {
    pub payment_hash: Hash256,
    pub next_node_id: Pubkey,
    pub amount_to_forward: u128,
    pub hash_algorithm: HashAlgorithm,
    pub build_max_fee_amount: u128,
    pub tlc_expiry_delta: u64,
    pub tlc_expiry_limit: u64,
    pub max_parts: Option<u64>,
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
    pub remaining_trampoline_onion: Vec<u8>,
    pub previous_tlc: PrevTlcInfo,
    pub max_outgoing_tlc_expiry: u64,
}

impl TrampolineForwardingRequest {
    /// Stable local session identity for hosted dispatch and crash recovery.
    pub fn hosted_session_key(&self) -> crate::fiber_types::PaymentSessionKey {
        crate::fiber_types::PaymentSessionKey {
            payment_hash: self.payment_hash,
            execution_id: Some(crate::fiber_types::UpstreamTlcKey {
                incoming_channel_id: self.previous_tlc.prev_channel_id,
                incoming_tlc_id: self.previous_tlc.prev_tlc_id,
            }),
        }
    }
    pub(crate) fn into_send_payment_data(self) -> Result<SendPaymentData, String> {
        SendPaymentDataBuilder::new(self.next_node_id, self.amount_to_forward, self.payment_hash)
            .final_tlc_expiry_delta(self.tlc_expiry_delta)
            .tlc_expiry_limit(self.tlc_expiry_limit)
            .max_fee_amount(Some(self.build_max_fee_amount))
            .max_parts(self.max_parts)
            .udt_type_script(self.udt_type_script)
            .trampoline_context(Some(TrampolineContext {
                remaining_trampoline_onion: self.remaining_trampoline_onion,
                // The current trampoline forwarding flow supports one upstream TLC.
                previous_tlcs: vec![self.previous_tlc],
                hash_algorithm: self.hash_algorithm,
                max_outgoing_tlc_expiry: Some(self.max_outgoing_tlc_expiry),
            }))
            .allow_mpp(self.max_parts.is_some_and(|value| value > 1))
            .build()
    }
}

/// The core's view of a hosted delivery service.
///
/// Implemented by `crate::lsp::LspDeliveryPort`. Nodes without hosted tenants
/// never install one, which is why every call site handles the `None` case.
#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait HostedDeliveryPort: std::fmt::Debug + Send + Sync + 'static {
    /// Ask whether a hosted tenant owns this incoming trampoline payment.
    async fn accept_trampoline_delivery(
        &self,
        request: TrampolineForwardingRequest,
    ) -> Result<DeliveryDecision, String>;

    /// Hand a final downstream outcome over before the upstream TLC is resolved.
    async fn decide_payment_outcome(
        &self,
        request: PaymentOutcomeRequest,
    ) -> Result<PaymentOutcomeDecision, String>;

    /// Report that the upstream TLC was resolved with this outcome.
    fn report_settled_payment(&self, outcome: SettledPaymentOutcome);

    /// Report a tenant channel going online or offline.
    fn report_tenant_channel(&self, tenant_pubkey: Pubkey, channel_id: Hash256, online: bool);
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::path::{Path, PathBuf};

    /// The core must not name the LSP module: tenancy is reached through
    /// [`HostedDeliveryPort`]. Without this the port erodes one `crate::lsp::`
    /// import at a time, and `lsp` can never become a feature.
    #[test]
    fn core_does_not_depend_on_the_lsp_module() {
        fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read core directory") {
                let path = entry.expect("directory entry").path();
                if path.is_dir() {
                    // Test modules may use LSP fixtures.
                    if path.file_name().and_then(|name| name.to_str()) != Some("tests") {
                        visit(&path, out);
                    }
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
                    && path.file_name().and_then(|name| name.to_str()) != Some("delivery.rs")
                {
                    // This module names the LSP in its documentation, which is the
                    // port's whole point.
                    out.push(path);
                }
            }
        }

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fiber");
        let mut files = Vec::new();
        visit(&root, &mut files);
        assert!(
            !files.is_empty(),
            "no core sources under {}",
            root.display()
        );

        let offenders: Vec<String> = files
            .iter()
            .filter_map(|path| {
                let source = std::fs::read_to_string(path).expect("read core source");
                let code_reaches_lsp = source
                    .lines()
                    .filter(|line| !line.trim_start().starts_with("//"))
                    .any(|line| line.contains("crate::lsp"));
                code_reaches_lsp.then(|| {
                    path.strip_prefix(&root)
                        .unwrap_or(path)
                        .display()
                        .to_string()
                })
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "these core files reach into crate::lsp; route them through \
             crate::fiber::delivery::HostedDeliveryPort instead: {offenders:?}"
        );
    }
}
