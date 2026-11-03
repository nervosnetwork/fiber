//! Adapter from the core's hosted-delivery port to the LSP service actor.
//!
//! [`HostedDeliveryPort`] is the core-side trait; this is the only place that
//! knows the answers come from [`LspServiceMessage`] round-trips.

use std::sync::Arc;

use ractor::ActorRef;

use crate::fiber::delivery::{
    DeliveryDecision, HostedDeliveryPort, PaymentOutcomeDecision, PaymentOutcomeRequest,
    SettledPaymentOutcome, TrampolineForwardingRequest,
};
use crate::fiber_types::{Hash256, Pubkey};

use super::LspServiceMessage;

/// Upper bound on how long the core waits for a delivery decision.
const DELIVERY_CALL_TIMEOUT_MS: u64 = 5_000;

/// [`HostedDeliveryPort`] backed by the LSP service actor.
#[derive(Debug)]
pub struct LspDeliveryPort {
    actor: ActorRef<LspServiceMessage>,
}

impl LspDeliveryPort {
    /// Wrap the running LSP service actor as the core's delivery port.
    pub fn new(actor: ActorRef<LspServiceMessage>) -> Arc<Self> {
        Arc::new(Self { actor })
    }
}

#[async_trait::async_trait]
impl HostedDeliveryPort for LspDeliveryPort {
    async fn accept_trampoline_delivery(
        &self,
        request: TrampolineForwardingRequest,
    ) -> Result<DeliveryDecision, String> {
        ractor::call_t!(
            self.actor,
            |reply| LspServiceMessage::AcceptTrampolineDelivery(request, reply),
            DELIVERY_CALL_TIMEOUT_MS
        )
        .map_err(|error| format!("LSP delivery service unavailable: {error}"))?
    }

    async fn decide_payment_outcome(
        &self,
        request: PaymentOutcomeRequest,
    ) -> Result<PaymentOutcomeDecision, String> {
        ractor::call_t!(
            self.actor,
            |reply| LspServiceMessage::PaymentOutcomeReady(request, reply),
            DELIVERY_CALL_TIMEOUT_MS
        )
        .map_err(|error| format!("LSP delivery service unavailable: {error}"))?
    }

    fn report_settled_payment(&self, outcome: SettledPaymentOutcome) {
        let _ = self
            .actor
            .send_message(LspServiceMessage::PaymentOutcomeSettled(outcome));
    }

    fn report_tenant_channel(&self, tenant_pubkey: Pubkey, channel_id: Hash256, online: bool) {
        let message = if online {
            LspServiceMessage::TenantChannelOnline(tenant_pubkey, channel_id)
        } else {
            LspServiceMessage::TenantChannelOffline(tenant_pubkey, channel_id)
        };
        let _ = self.actor.send_message(message);
    }
}
