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
use crate::fiber_types::{Hash256, PaymentSessionKey, Pubkey};
use crate::store::{NodeNamespace, Store};

use super::{
    LspInvoiceRegistry, LspPaymentDeliveryKey, LspPaymentDeliveryManager, LspServiceMessage,
};

/// Upper bound on how long the core waits for a delivery decision.
/// Must exceed slow downstream RPC waits inside `LspService` (such as 10s dispatch calls).
const DELIVERY_CALL_TIMEOUT_MS: u64 = 15_000;

/// [`HostedDeliveryPort`] backed by the LSP service actor.
#[derive(Debug)]
pub struct LspDeliveryPort {
    actor: ActorRef<LspServiceMessage>,
    /// Read-only view of `NodeNamespace::lsp_metadata()` used to short-circuit
    /// unrelated (non-hosted) trampoline forwards without waiting on the shared
    /// `LspService` actor mailbox.
    metadata_store: Option<Store>,
}

impl LspDeliveryPort {
    /// Wrap the running LSP service actor as the core's delivery port, using `store`
    /// to bypass the LSP actor mailbox for non-hosted trampoline forwards.
    pub fn new(actor: ActorRef<LspServiceMessage>, store: Store) -> Arc<Self> {
        Arc::new(Self {
            actor,
            metadata_store: Some(store.namespaced(NodeNamespace::lsp_metadata())),
        })
    }

    #[cfg(test)]
    pub(crate) fn without_fast_path(actor: ActorRef<LspServiceMessage>) -> Arc<Self> {
        Arc::new(Self {
            actor,
            metadata_store: None,
        })
    }

    fn has_active_hosted_delivery(
        store: &Store,
        session_key: PaymentSessionKey,
    ) -> Result<bool, String> {
        let Some(id) = session_key.upstream_tlc_key else {
            return Ok(false);
        };
        let key = LspPaymentDeliveryKey {
            incoming_channel_id: id.incoming_channel_id,
            incoming_tlc_id: id.incoming_tlc_id,
        };
        Ok(LspPaymentDeliveryManager::new(store.clone())
            .get(&key)?
            .is_some_and(|delivery| {
                delivery.payment_hash == session_key.payment_hash && !delivery.status.is_final()
            }))
    }
}

#[async_trait::async_trait]
impl HostedDeliveryPort for LspDeliveryPort {
    async fn accept_trampoline_delivery(
        &self,
        request: TrampolineForwardingRequest,
    ) -> Result<DeliveryDecision, String> {
        if let Some(store) = &self.metadata_store {
            if let Ok(None) = LspInvoiceRegistry::new(store.clone())
                .get(&request.next_node_id, &request.payment_hash)
            {
                return Ok(DeliveryDecision::NotHosted);
            }
        }
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
        if let Some(store) = &self.metadata_store {
            if let Ok(false) = Self::has_active_hosted_delivery(store, request.session_key) {
                return Ok(PaymentOutcomeDecision::SettleUpstream);
            }
        }
        ractor::call_t!(
            self.actor,
            |reply| LspServiceMessage::PaymentOutcomeReady(request, reply),
            DELIVERY_CALL_TIMEOUT_MS
        )
        .map_err(|error| format!("LSP delivery service unavailable: {error}"))?
    }

    fn report_settled_payment(&self, outcome: SettledPaymentOutcome) {
        if let Some(store) = &self.metadata_store {
            if let Ok(false) = Self::has_active_hosted_delivery(store, outcome.session_key) {
                return;
            }
        }
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
