use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
use tokio::task::AbortHandle;

use crate::fiber::{
    channel::{ChannelActorState, ChannelActorStateStore},
    delivery::{
        DeliveryDecision, DeliveryDispatchError, PaymentOutcomeDecision, PaymentOutcomeRequest,
        SettledPaymentOutcome, TrampolineForwardingRequest,
    },
    network::{BufferedTrampolineUpstreamStatus, FiberActorCommand, NetworkActorMessage},
};
use crate::fiber_types::{
    ChannelState, Hash256, PaymentStatus, Privkey, Pubkey, TenantRegistryPayload,
    TenantRegistrySignature, TlcErrorCode,
};
use crate::invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceStore};
use crate::store::{NodeNamespace, Store};

use super::{
    is_permanent_hosted_payment_failure, select_tenant_channel, tenant_watchtower_node_id,
    BiscuitTokenIssuer, HostedTenantChannelStatus, HostedTenantRpcContext, HostedTenantStatus,
    LspConfig, LspInvoiceRegistration, LspInvoiceRegistry, LspPaymentDelivery,
    LspPaymentDeliveryKey, LspPaymentDeliveryLimits, LspPaymentDeliveryManager,
    LspPaymentDeliveryStatus, TenantChannelRecord, TenantId, TenantRegistry, TenantRuntimeFactory,
    TenantRuntimeStatus, TenantSupervisor,
};

/// Runtime dependencies of the LSP service container.
pub struct LspServiceArgs {
    pub config: LspConfig,
    pub public_node_id: Pubkey,
    pub public_network_actor: ActorRef<NetworkActorMessage>,
    pub store: Store,
    pub runtime_factory: Arc<dyn TenantRuntimeFactory>,
    pub signing_key: Privkey,
    /// Issues tenant access tokens on authenticated registration.
    pub token_issuer: BiscuitTokenIssuer,
    /// Un-namespaced Public T store used for hosted watchtower rows.
    pub watchtower_store: Store,
    /// Application business event publisher.
    pub signing_events: crate::signing::SigningEvents,
}

/// Read-only status for callers that need to discover the hosted service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspServiceStatus {
    pub public_node_id: Pubkey,
    pub tenant_store_root: std::path::PathBuf,
    pub registered_tenants: usize,
    pub active_tenants: usize,
}

/// Result of hosted tenant registration or authenticated credential refresh.
#[derive(Debug)]
pub struct HostedTenantRegistration {
    pub status: HostedTenantStatus,
    pub created: bool,
    /// Tenant Biscuit issued for this newly created tenant.
    pub access_token: String,
    /// Exclusive token expiry in Unix milliseconds.
    pub expires_at_ms: u64,
}

/// Cold stores resolved from an authenticated tenant without activating its runtime.
#[derive(Clone, Debug)]
pub struct TenantSubscriptionContext {
    /// Channel state in the tenant namespace.
    pub store: Store,
    /// Shared watchtower store; access is restricted by `node_id`.
    pub watchtower_store: Store,
    /// Watchtower owner derived from the registered tenant.
    pub node_id: fiber_types::NodeId,
}

/// Commands accepted by the LSP service container.
pub enum LspServiceMessage {
    GetStatus(RpcReplyPort<LspServiceStatus>),
    GetPublicNodeId(RpcReplyPort<Pubkey>),
    GetWatchtowerQuotas(RpcReplyPort<crate::lsp_config::WatchtowerQuotas>),
    CheckTenantWatchtowerChannelQuota(TenantId, RpcReplyPort<Result<(), String>>),
    /// Atomically checks watchtower channel quota and reserves an in-flight opening slot
    /// for `tenant_id` until released or confirmed by the caller.
    ReserveTenantWatchtowerChannelQuota {
        tenant_id: TenantId,
        opening_temp_channel_id: Option<Hash256>,
        reply: RpcReplyPort<Result<u64, String>>,
    },
    /// Releases a previously acquired watchtower quota reservation and refreshes only
    /// `tenant_id`'s cached pending unwatched opening count from store.
    ReleaseTenantWatchtowerChannelQuota {
        tenant_id: TenantId,
        reservation_id: u64,
        opened_channel_id: Option<Hash256>,
    },
    /// Recomputes only `tenant_id`'s cached pending unwatched opening count from store.
    RefreshTenantWatchtowerChannelQuota(TenantId),
    /// Recomputes the cached pending unwatched opening count for the tenant owning `node_id`.
    RefreshTenantWatchtowerChannelQuotaByNodeId(fiber_types::NodeId),
    GetTenantTokenEpochs(RpcReplyPort<crate::rpc::biscuit::TenantTokenEpochs>),
    RevokeTenantTokens(TenantId, RpcReplyPort<Result<u64, String>>),
    RegisterAuthenticatedTenant {
        payload: TenantRegistryPayload,
        signature: TenantRegistrySignature,
        reply: RpcReplyPort<Result<HostedTenantRegistration, String>>,
    },
    EnsureTenant(TenantId, RpcReplyPort<Result<HostedTenantStatus, String>>),
    EvictTenant(TenantId, RpcReplyPort<Result<HostedTenantStatus, String>>),
    ListTenants(RpcReplyPort<Result<Vec<HostedTenantStatus>, String>>),
    /// Returns an active tenant's scoped RPC backend, starting it when needed.
    GetTenantRpcContext(
        TenantId,
        RpcReplyPort<Result<HostedTenantRpcContext, String>>,
    ),
    /// Reads a tenant's channel signing status directly from store without activating runtime.
    GetTenantSigningStatus {
        tenant_id: TenantId,
        channel_id: Hash256,
        reply: RpcReplyPort<Result<ChannelActorState, String>>,
    },
    /// Resolve cold subscription stores without hydrating a tenant runtime.
    GetTenantSubscriptionContext(
        TenantId,
        RpcReplyPort<Result<TenantSubscriptionContext, String>>,
    ),
    RegisterInvoice {
        tenant_id: TenantId,
        invoice: CkbInvoice,
        buffer_duration_ms: Option<u64>,
        reply: RpcReplyPort<Result<LspInvoiceRegistration, String>>,
    },
    InvoiceCancelled {
        tenant_id: TenantId,
        payment_hash: Hash256,
    },
    GetPaymentDelivery(
        Hash256,
        RpcReplyPort<Result<Option<LspPaymentDelivery>, String>>,
    ),
    AcceptTrampolineDelivery(
        TrampolineForwardingRequest,
        RpcReplyPort<Result<DeliveryDecision, String>>,
    ),
    ResumeDelivery(LspPaymentDeliveryKey),
    ExpireDelivery(LspPaymentDeliveryKey),
    ReconcileInFlightDeliveries,
    TenantChannelOnline(Pubkey, Hash256),
    TenantChannelOffline(Pubkey, Hash256),
    PaymentOutcomeReady(
        PaymentOutcomeRequest,
        RpcReplyPort<Result<PaymentOutcomeDecision, String>>,
    ),
    PaymentOutcomeSettled(SettledPaymentOutcome),
}

/// In-flight watchtower quota slot held while an opening RPC is executing.
#[cfg(feature = "watchtower")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InFlightQuotaReservation {
    reservation_id: u64,
    opening_temp_channel_id: Option<Hash256>,
}

/// Top-level container for the multi-tenant LSP subsystem.
pub struct LspService;

/// State owned by the LSP service. Tenant components are added behind this
/// boundary rather than sharing Public T's network actor or unscoped keyspace.
pub struct LspServiceState {
    pub config: LspConfig,
    pub public_node_id: Pubkey,
    pub public_network_actor: ActorRef<NetworkActorMessage>,
    pub store: Store,
    pub registry: TenantRegistry<Store>,
    pub invoice_registry: LspInvoiceRegistry<Store>,
    pub delivery_manager: LspPaymentDeliveryManager<Store>,
    pub supervisor: TenantSupervisor,
    pub signing_key: Privkey,
    pub token_issuer: BiscuitTokenIssuer,
    pub watchtower_store: Store,
    #[cfg(feature = "watchtower")]
    signing_events: crate::signing::SigningEvents,
    pub ready_tenants: HashMap<TenantId, HashSet<Hash256>>,
    pub token_epochs: crate::rpc::biscuit::TenantTokenEpochs,
    // Deduplicates in-flight/settling reconciliation timers so periodic ticks
    // never amplify per-delivery retry loops.
    reconciling_deliveries: HashSet<LspPaymentDeliveryKey>,
    // Deduplicates pending delivery retry timers so overlapping failure paths
    // (e.g., temporary dispatch failure and PaymentOutcomeReady) arm only one timer.
    scheduled_delivery_retries: HashMap<LspPaymentDeliveryKey, AbortHandle>,
    // Cached per-tenant effective pending unwatched channel counts (persisted openings
    // unioned with in-flight reservations), keeping global quota checks O(1) across tenants.
    #[cfg(feature = "watchtower")]
    pending_unwatched_by_tenant: HashMap<TenantId, usize>,
    #[cfg(feature = "watchtower")]
    pending_unwatched_total: usize,
    #[cfg(feature = "watchtower")]
    in_flight_quota_reservations: HashMap<TenantId, Vec<InFlightQuotaReservation>>,
    #[cfg(feature = "watchtower")]
    next_quota_reservation_id: u64,
}

impl LspServiceState {
    fn delivery_for_outcome(
        &self,
        session_key: crate::fiber_types::PaymentSessionKey,
    ) -> Result<Option<LspPaymentDelivery>, String> {
        let Some(id) = session_key.upstream_tlc_key else {
            return Ok(None);
        };
        let key = LspPaymentDeliveryKey {
            incoming_channel_id: id.incoming_channel_id,
            incoming_tlc_id: id.incoming_tlc_id,
        };
        Ok(self.delivery_manager.get(&key)?.filter(|delivery| {
            delivery.payment_hash == session_key.payment_hash && !delivery.status.is_final()
        }))
    }

    fn tenant_delivery_channels(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<TenantChannelRecord>, String> {
        let mut channels = self.registry.list_channels(tenant_id)?;
        // Bindings outlive the channel, and an Online notification can precede
        // shutdown. Public T's persisted state is authoritative for its ability
        // to send new TLCs; an offline but nonterminal channel may still recover.
        channels.retain(|channel| {
            self.watchtower_store
                .get_channel_actor_state(&channel.channel_id)
                .is_none_or(|state| {
                    !matches!(
                        state.state,
                        ChannelState::Closed(_) | ChannelState::ShuttingDown(_)
                    )
                })
        });
        Ok(channels)
    }

    fn invoice_cancelled(&self, delivery: &LspPaymentDelivery) -> bool {
        // Registration records do not track lifecycle changes; the tenant's invoice store does.
        self.store
            .namespaced(NodeNamespace::hosted_tenant(delivery.tenant_id.as_str()))
            .get_invoice_status(&delivery.payment_hash)
            == Some(CkbInvoiceStatus::Cancelled)
    }

    fn tenant_status(&self, record: crate::lsp::HostedTenantRecord) -> HostedTenantStatus {
        let runtime_status = if self.supervisor.is_active(&record.tenant_id) {
            TenantRuntimeStatus::Active
        } else {
            TenantRuntimeStatus::Cold
        };
        let ready = self.ready_tenants.get(&record.tenant_id);
        let channels = self
            .registry
            .list_channels(&record.tenant_id)
            .unwrap_or_default()
            .into_iter()
            .map(|channel| HostedTenantChannelStatus {
                online: ready.is_some_and(|ready| ready.contains(&channel.channel_id)),
                channel,
            })
            .collect::<Vec<_>>();
        HostedTenantStatus {
            record,
            runtime_status,
            channels,
        }
    }

    fn get_tenant_status(&self, tenant_id: &TenantId) -> Result<HostedTenantStatus, String> {
        self.registry
            .get(tenant_id)?
            .map(|record| self.tenant_status(record))
            .ok_or_else(|| format!("tenant {tenant_id} is not registered"))
    }

    #[cfg(feature = "watchtower")]
    fn persisted_pending_unwatched_ids_for_tenant(
        &self,
        tenant: &crate::lsp::HostedTenantRecord,
    ) -> HashSet<Hash256> {
        use crate::fiber::channel::{derive_temp_channel_id_from_tlc_key, ChannelOpenRecordStore};
        use crate::fiber_types::{ChannelOpeningStatus, NegotiatingFundingFlags};
        use crate::watchtower::WatchtowerStore;

        let node_id = tenant_watchtower_node_id(&tenant.tenant_pubkey);
        let tenant_store = self
            .store
            .namespaced(NodeNamespace::hosted_tenant(tenant.tenant_id.as_str()));
        let tenant_active = self.supervisor.is_active(&tenant.tenant_id);
        let mut pending_ids = HashSet::new();
        let mut seen_opening_ids = HashSet::new();

        for opening in tenant_store.list_tenant_channel_openings() {
            let temp_id = opening
                .command
                .external_channel_signer
                .as_ref()
                .map(|signer| {
                    derive_temp_channel_id_from_tlc_key(&signer.base_public_keys.tlc_base_key)
                });
            seen_opening_ids.insert(opening.channel_id);
            if let Some(id) = temp_id {
                seen_opening_ids.insert(id);
            }
            if opening.terminated
                || self
                    .watchtower_store
                    .get_watch_channel(&node_id, &opening.channel_id)
                    .is_some()
            {
                continue;
            }
            let open_record = tenant_store.get_channel_open_record(&opening.channel_id);
            if open_record.as_ref().is_some_and(|record| {
                matches!(
                    record.status,
                    ChannelOpeningStatus::Failed | ChannelOpeningStatus::ChannelReady
                )
            }) {
                continue;
            }
            let channel_state = tenant_store.get_channel_actor_state(&opening.channel_id);
            if channel_state
                .as_ref()
                .is_some_and(|channel| channel.is_closed())
            {
                continue;
            }
            let stale_pre_funding = opening.result.is_none()
                && temp_id.is_some_and(|id| opening.channel_id != id)
                && channel_state.as_ref().is_some_and(|channel| {
                    channel.reestablishing
                        && channel.external_funding.is_none()
                        && matches!(
                            channel.state,
                            ChannelState::NegotiatingFunding(flags)
                                if !flags.contains(NegotiatingFundingFlags::AWAITING_EXTERNAL_FUNDING)
                        )
                        && channel.session.outgoing.is_none()
                        && channel.session.incoming.is_none()
                        && channel.session.pending_incoming.is_none()
                });
            if stale_pre_funding {
                continue;
            }
            if opening.result.is_some() || channel_state.is_some() || tenant_active {
                pending_ids.insert(temp_id.unwrap_or(opening.channel_id));
            }
        }

        // Also count standalone non-terminal ChannelOpenRecords (e.g. from `open_channel`
        // without a TenantChannelOpening) so openings stay counted across temp -> final
        // channel ID transitions until watched, failed, or closed.
        for record in tenant_store.get_channel_open_records() {
            if seen_opening_ids.contains(&record.channel_id) {
                continue;
            }
            if matches!(
                record.status,
                ChannelOpeningStatus::Failed | ChannelOpeningStatus::ChannelReady
            ) {
                continue;
            }
            if self
                .watchtower_store
                .get_watch_channel(&node_id, &record.channel_id)
                .is_some()
            {
                continue;
            }
            let channel_state = tenant_store.get_channel_actor_state(&record.channel_id);
            if channel_state
                .as_ref()
                .is_some_and(|channel| channel.is_closed())
            {
                continue;
            }
            if channel_state.is_some() || tenant_active {
                pending_ids.insert(record.channel_id);
            }
        }

        pending_ids
    }

    #[cfg(feature = "watchtower")]
    fn set_tenant_pending_unwatched(&mut self, tenant_id: &TenantId, new_count: usize) {
        let old_count = if new_count == 0 {
            self.pending_unwatched_by_tenant
                .remove(tenant_id)
                .unwrap_or(0)
        } else {
            self.pending_unwatched_by_tenant
                .insert(tenant_id.clone(), new_count)
                .unwrap_or(0)
        };
        self.pending_unwatched_total = self
            .pending_unwatched_total
            .saturating_sub(old_count)
            .saturating_add(new_count);
    }

    #[cfg(feature = "watchtower")]
    fn compute_and_cache_tenant_pending(
        &mut self,
        tenant_id: &TenantId,
        persisted_ids: &HashSet<Hash256>,
    ) -> usize {
        let unmatched_in_flight = self
            .in_flight_quota_reservations
            .get(tenant_id)
            .map(|reservations| {
                reservations
                    .iter()
                    .filter(|res| {
                        res.opening_temp_channel_id
                            .is_none_or(|temp_id| !persisted_ids.contains(&temp_id))
                    })
                    .count()
            })
            .unwrap_or(0);
        let effective = persisted_ids.len().saturating_add(unmatched_in_flight);
        self.set_tenant_pending_unwatched(tenant_id, effective);
        effective
    }

    #[cfg(feature = "watchtower")]
    fn refresh_tenant_pending_unwatched(
        &mut self,
        tenant: &crate::lsp::HostedTenantRecord,
    ) -> usize {
        let persisted_ids = self.persisted_pending_unwatched_ids_for_tenant(tenant);
        self.compute_and_cache_tenant_pending(&tenant.tenant_id, &persisted_ids)
    }

    #[cfg(feature = "watchtower")]
    fn refresh_tenant_pending_unwatched_by_id(&mut self, tenant_id: &TenantId) {
        match self.registry.get(tenant_id) {
            Ok(Some(tenant)) => {
                self.refresh_tenant_pending_unwatched(&tenant);
            }
            Ok(None) => {
                self.set_tenant_pending_unwatched(tenant_id, 0);
            }
            Err(error) => {
                tracing::warn!(
                    %tenant_id,
                    %error,
                    "Failed to load tenant while refreshing pending watchtower quota count"
                );
            }
        }
    }

    #[cfg(feature = "watchtower")]
    fn refresh_tenant_pending_unwatched_by_node_id(&mut self, node_id: &fiber_types::NodeId) {
        let Ok(tenant_pubkey) = Pubkey::from_slice(node_id.as_ref()) else {
            return;
        };
        match self.registry.find_by_tenant_pubkey(&tenant_pubkey) {
            Ok(Some(tenant)) => {
                self.refresh_tenant_pending_unwatched(&tenant);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    ?tenant_pubkey,
                    %error,
                    "Failed to load tenant while refreshing pending watchtower quota count by node_id"
                );
            }
        }
    }

    fn check_tenant_watchtower_channel_quota(
        &mut self,
        tenant_id: &TenantId,
    ) -> Result<(), String> {
        let tenant = self
            .registry
            .get(tenant_id)?
            .ok_or_else(|| format!("tenant {tenant_id} is not registered"))?;
        #[cfg(feature = "watchtower")]
        {
            use crate::watchtower::WatchtowerStore;

            let node_id = tenant_watchtower_node_id(&tenant.tenant_pubkey);
            let quotas = self.config.watchtower_quotas();
            let pending_node = self.refresh_tenant_pending_unwatched(&tenant);
            let pending_total = self.pending_unwatched_total;
            self.watchtower_store
                .check_watch_channel_quota_with_pending(
                    &node_id,
                    pending_node,
                    pending_total,
                    &quotas,
                )?;
        }
        #[cfg(not(feature = "watchtower"))]
        let _ = tenant;
        Ok(())
    }

    fn reserve_tenant_watchtower_channel_quota(
        &mut self,
        tenant_id: &TenantId,
        opening_temp_channel_id: Option<Hash256>,
    ) -> Result<u64, String> {
        let tenant = self
            .registry
            .get(tenant_id)?
            .ok_or_else(|| format!("tenant {tenant_id} is not registered"))?;
        #[cfg(feature = "watchtower")]
        {
            use crate::watchtower::WatchtowerStore;

            let node_id = tenant_watchtower_node_id(&tenant.tenant_pubkey);
            let quotas = self.config.watchtower_quotas();
            let persisted_ids = self.persisted_pending_unwatched_ids_for_tenant(&tenant);
            let pending_node =
                self.compute_and_cache_tenant_pending(&tenant.tenant_id, &persisted_ids);
            let pending_total = self.pending_unwatched_total;
            self.watchtower_store
                .check_watch_channel_quota_with_pending(
                    &node_id,
                    pending_node,
                    pending_total,
                    &quotas,
                )?;
            let reservation_id = self.next_quota_reservation_id;
            self.next_quota_reservation_id = self.next_quota_reservation_id.wrapping_add(1).max(1);
            self.in_flight_quota_reservations
                .entry(tenant.tenant_id.clone())
                .or_default()
                .push(InFlightQuotaReservation {
                    reservation_id,
                    opening_temp_channel_id,
                });
            self.compute_and_cache_tenant_pending(&tenant.tenant_id, &persisted_ids);
            Ok(reservation_id)
        }
        #[cfg(not(feature = "watchtower"))]
        {
            let _ = (tenant, opening_temp_channel_id);
            Ok(0)
        }
    }

    fn release_tenant_watchtower_channel_quota(
        &mut self,
        tenant_id: &TenantId,
        reservation_id: u64,
        _opened_channel_id: Option<Hash256>,
    ) {
        #[cfg(feature = "watchtower")]
        {
            if let Some(reservations) = self.in_flight_quota_reservations.get_mut(tenant_id) {
                reservations.retain(|res| res.reservation_id != reservation_id);
                if reservations.is_empty() {
                    self.in_flight_quota_reservations.remove(tenant_id);
                }
            }
            self.refresh_tenant_pending_unwatched_by_id(tenant_id);
        }
        #[cfg(not(feature = "watchtower"))]
        let _ = (tenant_id, reservation_id, _opened_channel_id);
    }

    fn issue_tenant_token(
        &self,
        record: &crate::lsp::HostedTenantRecord,
    ) -> Result<(String, u64), String> {
        let expires_at_ms = (crate::now_timestamp_as_millis_u64() / 1_000)
            .checked_add(self.config.token_ttl_secs)
            .and_then(|seconds| seconds.checked_mul(1_000))
            .ok_or_else(|| "tenant token expiry overflow".to_string())?;
        let access_token = self
            .token_issuer
            .issue_tenant_token(
                &record.tenant_id,
                &tenant_watchtower_node_id(&record.tenant_pubkey),
                expires_at_ms / 1_000,
                record.token_epoch,
            )
            .map_err(|error| error.to_string())?;
        Ok((access_token, expires_at_ms))
    }

    fn schedule_delivery_deadline(
        myself: &ActorRef<LspServiceMessage>,
        key: LspPaymentDeliveryKey,
        deadline: u64,
    ) {
        let delay = deadline.saturating_sub(crate::now_timestamp_as_millis_u64());
        myself.send_after(Duration::from_millis(delay), move || {
            LspServiceMessage::ExpireDelivery(key)
        });
    }

    fn cancel_scheduled_delivery_retry(&mut self, key: &LspPaymentDeliveryKey) {
        if let Some(handle) = self.scheduled_delivery_retries.remove(key) {
            handle.abort();
        }
    }

    fn schedule_delivery_retry(
        &mut self,
        myself: &ActorRef<LspServiceMessage>,
        key: LspPaymentDeliveryKey,
        deadline: u64,
    ) {
        let remaining = deadline.saturating_sub(crate::now_timestamp_as_millis_u64());
        if remaining == 0 {
            self.cancel_scheduled_delivery_retry(&key);
            let _ = myself.send_message(LspServiceMessage::ExpireDelivery(key));
        } else if let Entry::Vacant(entry) = self.scheduled_delivery_retries.entry(key) {
            let handle = myself
                .send_after(Duration::from_millis(remaining.min(1_000)), move || {
                    LspServiceMessage::ResumeDelivery(key)
                });
            entry.insert(handle.abort_handle());
        }
    }

    fn schedule_reconciliation_retry(
        &mut self,
        myself: &ActorRef<LspServiceMessage>,
        key: LspPaymentDeliveryKey,
    ) {
        if self.reconciling_deliveries.insert(key) {
            myself.send_after(Duration::from_secs(1), move || {
                LspServiceMessage::ResumeDelivery(key)
            });
        }
    }

    fn record_payment_outcome(
        &self,
        key: &LspPaymentDeliveryKey,
        status: PaymentStatus,
        failure: Option<String>,
        failure_code: Option<TlcErrorCode>,
    ) -> Result<PaymentOutcomeDecision, String> {
        let delivery = self
            .delivery_manager
            .get(key)?
            .ok_or_else(|| format!("hosted payment delivery {key:?} not found"))?;
        if let LspPaymentDeliveryStatus::SettlingUpstream { payment_status, .. } = delivery.status {
            // A settlement decision is durable, including a buffer timeout represented by a
            // temporary failure code. Re-reading that session must not restart delivery or
            // replace the decision just because its error description differs.
            if payment_status == status {
                return Ok(PaymentOutcomeDecision::SettleUpstream);
            }
        }
        let error = failure.clone().map(|reason| (reason, failure_code));
        if status == PaymentStatus::Failed
            && failure_code.is_some_and(|code| !is_permanent_hosted_payment_failure(code))
        {
            self.delivery_manager.transition_with_error(
                key,
                LspPaymentDeliveryStatus::Deferred,
                error,
                crate::now_timestamp_as_millis_u64(),
            )?;
            return Ok(PaymentOutcomeDecision::RetryDelivery);
        }
        let delivery_status = match status {
            PaymentStatus::Created | PaymentStatus::Inflight => LspPaymentDeliveryStatus::InFlight,
            PaymentStatus::Success | PaymentStatus::Failed => {
                LspPaymentDeliveryStatus::SettlingUpstream {
                    payment_status: status,
                    failure,
                }
            }
        };
        self.delivery_manager.transition_with_error(
            key,
            delivery_status,
            error,
            crate::now_timestamp_as_millis_u64(),
        )?;
        Ok(PaymentOutcomeDecision::SettleUpstream)
    }

    fn finish_upstream_settlement(
        &mut self,
        key: &LspPaymentDeliveryKey,
        status: PaymentStatus,
        failure: Option<String>,
    ) -> Result<(), String> {
        let delivery_status = match status {
            PaymentStatus::Created | PaymentStatus::Inflight => {
                return Err(format!(
                    "hosted payment delivery {key:?} has no final downstream outcome"
                ));
            }
            PaymentStatus::Success => LspPaymentDeliveryStatus::Succeeded,
            PaymentStatus::Failed => LspPaymentDeliveryStatus::Failed {
                reason: failure.unwrap_or_else(|| "downstream payment failed".to_string()),
            },
        };
        self.delivery_manager.transition(
            key,
            delivery_status,
            crate::now_timestamp_as_millis_u64(),
        )?;
        self.cancel_scheduled_delivery_retry(key);
        self.reconciling_deliveries.remove(key);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Actor for LspService {
    type Msg = LspServiceMessage;
    type State = LspServiceState;
    type Arguments = LspServiceArgs;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        args.config.validate().map_err(anyhow::Error::msg)?;
        let registry = TenantRegistry::new(args.store.clone());
        let invoice_registry = LspInvoiceRegistry::with_max_buffer_duration(
            args.store.clone(),
            args.config.max_buffer_duration_ms,
        );
        let delivery_manager = LspPaymentDeliveryManager::with_limits(
            args.store.clone(),
            LspPaymentDeliveryLimits {
                max_pending_deliveries: args.config.max_pending_deliveries,
                max_pending_deliveries_per_tenant: args.config.max_pending_deliveries_per_tenant,
            },
        );
        args.runtime_factory.set_lsp_service(myself.clone());
        let supervisor =
            TenantSupervisor::new(args.runtime_factory.clone(), args.config.max_active_tenants);
        for delivery in delivery_manager
            .list_pending()
            .map_err(anyhow::Error::msg)?
        {
            let key = delivery.key();
            let _ = myself.send_message(LspServiceMessage::ResumeDelivery(key));
            LspServiceState::schedule_delivery_deadline(&myself, key, delivery.buffer_deadline);
        }
        // Periodically reconcile InFlight / SettlingUpstream deliveries so a transient
        // outcome-decision timeout or dropped notification cannot stall a delivery.
        myself.send_interval(Duration::from_secs(5), || {
            LspServiceMessage::ReconcileInFlightDeliveries
        });
        let tenants = registry.list().map_err(anyhow::Error::msg)?;
        let token_epochs = crate::rpc::biscuit::TenantTokenEpochs::from_map(
            tenants
                .iter()
                .filter(|record| record.token_epoch > 0)
                .map(|record| (record.tenant_id.clone(), record.token_epoch))
                .collect(),
        );
        #[allow(unused_mut)]
        let mut state = LspServiceState {
            config: args.config,
            public_node_id: args.public_node_id,
            public_network_actor: args.public_network_actor,
            store: args.store,
            registry,
            invoice_registry,
            delivery_manager,
            supervisor,
            signing_key: args.signing_key,
            token_issuer: args.token_issuer,
            watchtower_store: args.watchtower_store,
            #[cfg(feature = "watchtower")]
            signing_events: args.signing_events,
            ready_tenants: HashMap::new(),
            token_epochs,
            reconciling_deliveries: HashSet::new(),
            scheduled_delivery_retries: HashMap::new(),
            #[cfg(feature = "watchtower")]
            pending_unwatched_by_tenant: HashMap::new(),
            #[cfg(feature = "watchtower")]
            pending_unwatched_total: 0,
            #[cfg(feature = "watchtower")]
            in_flight_quota_reservations: HashMap::new(),
            #[cfg(feature = "watchtower")]
            next_quota_reservation_id: 1,
        };
        #[cfg(feature = "watchtower")]
        for tenant in &tenants {
            state.refresh_tenant_pending_unwatched(tenant);
        }
        Ok(state)
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            LspServiceMessage::GetTenantSubscriptionContext(tenant_id, reply) => {
                let result = state.registry.get(&tenant_id).and_then(|record| {
                    let record =
                        record.ok_or_else(|| format!("tenant {tenant_id} is not registered"))?;
                    Ok(TenantSubscriptionContext {
                        store: state
                            .store
                            .namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str())),
                        watchtower_store: state.watchtower_store.clone(),
                        node_id: tenant_watchtower_node_id(&record.tenant_pubkey),
                    })
                });
                let _ = reply.send(result);
            }
            LspServiceMessage::GetStatus(reply) => {
                let registered_tenants = state.registry.count().unwrap_or(0);
                let _ = reply.send(LspServiceStatus {
                    public_node_id: state.public_node_id,
                    tenant_store_root: state.config.tenant_store_root(),
                    registered_tenants,
                    active_tenants: state.supervisor.active_count(),
                });
            }
            LspServiceMessage::GetPublicNodeId(reply) => {
                let _ = reply.send(state.public_node_id);
            }
            LspServiceMessage::GetWatchtowerQuotas(reply) => {
                let _ = reply.send(state.config.watchtower_quotas());
            }
            LspServiceMessage::CheckTenantWatchtowerChannelQuota(tenant_id, reply) => {
                let _ = reply.send(state.check_tenant_watchtower_channel_quota(&tenant_id));
            }
            LspServiceMessage::ReserveTenantWatchtowerChannelQuota {
                tenant_id,
                opening_temp_channel_id,
                reply,
            } => {
                let _ =
                    reply.send(state.reserve_tenant_watchtower_channel_quota(
                        &tenant_id,
                        opening_temp_channel_id,
                    ));
            }
            LspServiceMessage::ReleaseTenantWatchtowerChannelQuota {
                tenant_id,
                reservation_id,
                opened_channel_id,
            } => {
                state.release_tenant_watchtower_channel_quota(
                    &tenant_id,
                    reservation_id,
                    opened_channel_id,
                );
            }
            LspServiceMessage::RefreshTenantWatchtowerChannelQuota(tenant_id) => {
                #[cfg(feature = "watchtower")]
                state.refresh_tenant_pending_unwatched_by_id(&tenant_id);
                #[cfg(not(feature = "watchtower"))]
                let _ = tenant_id;
            }
            LspServiceMessage::RefreshTenantWatchtowerChannelQuotaByNodeId(node_id) => {
                #[cfg(feature = "watchtower")]
                state.refresh_tenant_pending_unwatched_by_node_id(&node_id);
                #[cfg(not(feature = "watchtower"))]
                let _ = node_id;
            }
            LspServiceMessage::GetTenantTokenEpochs(reply) => {
                let _ = reply.send(state.token_epochs.clone());
            }
            LspServiceMessage::RevokeTenantTokens(tenant_id, reply) => {
                let result = state
                    .registry
                    .revoke_tenant_tokens(&tenant_id)
                    .inspect(|&epoch| {
                        state.token_epochs.set(tenant_id, epoch);
                    });
                let _ = reply.send(result);
            }
            LspServiceMessage::RegisterAuthenticatedTenant {
                payload,
                signature,
                reply,
            } => {
                let result = (|| {
                    if payload.lsp_node_id != state.public_node_id {
                        return Err(
                            "tenant registration proof targets another LSP node".to_string()
                        );
                    }
                    payload.verify_signature(&signature).map_err(|error| {
                        format!("invalid RootSigner registration proof: {error}")
                    })?;
                    payload
                        .validate_signed_at(crate::now_timestamp_as_millis_u64())
                        .map_err(|error| {
                            format!("invalid RootSigner registration proof: {error}")
                        })?;
                    let tenant_id = TenantId::from_root_signer_pubkey(&payload.root_signer_pubkey);
                    let (record, created) = if state.registry.get(&tenant_id)?.is_some() {
                        // Reauthentication uses a new proof, and preserves all existing
                        // tenant state instead of provisioning a new protocol identity.
                        (
                            state.registry.refresh_authenticated(
                                payload.root_signer_pubkey,
                                payload.request_id,
                            )?,
                            false,
                        )
                    } else {
                        let mut record = state.supervisor.provision(&tenant_id)?;
                        record.root_signer_pubkey = Some(payload.root_signer_pubkey);
                        (
                            state
                                .registry
                                .register_authenticated(record, payload.request_id)?,
                            true,
                        )
                    };
                    let (access_token, expires_at_ms) = state.issue_tenant_token(&record)?;
                    Ok(HostedTenantRegistration {
                        status: state.tenant_status(record),
                        created,
                        access_token,
                        expires_at_ms,
                    })
                })();
                let _ = reply.send(result);
            }
            LspServiceMessage::EnsureTenant(tenant_id, reply) => {
                let result = match state.registry.get(&tenant_id) {
                    Ok(Some(record)) => state
                        .supervisor
                        .ensure(&record)
                        .await
                        .map(|()| state.tenant_status(record)),
                    Ok(None) => Err(format!("tenant {tenant_id} is not registered")),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            LspServiceMessage::EvictTenant(tenant_id, reply) => {
                let result = if state.delivery_manager.has_pending_for_tenant(&tenant_id)? {
                    Err(format!(
                        "tenant {tenant_id} has unfinished hosted payment deliveries"
                    ))
                } else {
                    match state.supervisor.evict(&tenant_id).await {
                        Ok(_) => {
                            state.ready_tenants.remove(&tenant_id);
                            #[cfg(feature = "watchtower")]
                            state.refresh_tenant_pending_unwatched_by_id(&tenant_id);
                            state.get_tenant_status(&tenant_id)
                        }
                        Err(error) => Err(format!("cannot evict tenant {tenant_id}: {error}")),
                    }
                };
                let _ = reply.send(result);
            }
            LspServiceMessage::ListTenants(reply) => {
                let result = state.registry.list().map(|records| {
                    records
                        .into_iter()
                        .map(|record| state.tenant_status(record))
                        .collect()
                });
                let _ = reply.send(result);
            }
            LspServiceMessage::GetTenantRpcContext(tenant_id, reply) => {
                let result = match state.registry.get(&tenant_id) {
                    Ok(Some(record)) => state
                        .supervisor
                        .ensure(&record)
                        .await
                        .and_then(|()| state.supervisor.rpc_context(&tenant_id)),
                    Ok(None) => Err(format!("tenant {tenant_id} is not registered")),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            LspServiceMessage::GetTenantSigningStatus {
                tenant_id,
                channel_id,
                reply,
            } => {
                let result = (|| {
                    if state.registry.get(&tenant_id)?.is_none() {
                        return Err(format!("tenant {tenant_id} is not registered"));
                    }
                    state
                        .store
                        .namespaced(NodeNamespace::hosted_tenant(tenant_id.as_str()))
                        .get_channel_actor_state(&channel_id)
                        .ok_or_else(|| format!("channel {channel_id:?} not found"))
                })();
                let _ = reply.send(result);
            }
            LspServiceMessage::RegisterInvoice {
                tenant_id,
                invoice,
                buffer_duration_ms,
                reply,
            } => {
                let result = match state.registry.get(&tenant_id) {
                    Ok(Some(tenant)) => state.invoice_registry.register(
                        &tenant,
                        invoice,
                        buffer_duration_ms,
                        state.public_node_id,
                        &state.signing_key,
                    ),
                    Ok(None) => Err(format!("tenant {tenant_id} is not registered")),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            LspServiceMessage::InvoiceCancelled {
                tenant_id,
                payment_hash,
            } => {
                // The tenant RPC has persisted the cancellation. Wake only its active
                // delivery; ResumeDelivery fails the upstream TLC if it is still buffered.
                if let Some(delivery) = state
                    .delivery_manager
                    .get_active_by_payment_hash(&payment_hash)?
                {
                    if delivery.tenant_id == tenant_id {
                        let _ =
                            myself.send_message(LspServiceMessage::ResumeDelivery(delivery.key()));
                    }
                }
            }
            LspServiceMessage::GetPaymentDelivery(payment_hash, reply) => {
                let _ = reply.send(state.delivery_manager.get_by_payment_hash(&payment_hash));
            }
            LspServiceMessage::AcceptTrampolineDelivery(request, reply) => {
                let result = match state
                    .invoice_registry
                    .get(&request.next_node_id, &request.payment_hash)
                {
                    Ok(None) => Ok(DeliveryDecision::NotHosted),
                    Ok(Some(registration)) => match state.registry.get(&registration.tenant_id) {
                        Ok(Some(_)) if registration.hint.payload.buffer_duration_ms == 0 => {
                            Ok(DeliveryDecision::NotHosted)
                        }
                        // A hosted invoice is only the target when the trampoline hop
                        // asks for this tenant. Another node may issue an invoice that
                        // reuses a hosted payment hash; taking that delivery over sends
                        // an onion the tenant cannot peel, failing a payment that was
                        // never ours. Hosted invoices are minted by the tenant runtime,
                        // so their payee is the tenant and this is the same check.
                        Ok(Some(tenant)) if request.next_node_id != tenant.tenant_pubkey => {
                            Ok(DeliveryDecision::NotHosted)
                        }
                        Ok(Some(tenant)) => (|| {
                            if let Some(existing) =
                                state.delivery_manager.existing_request(&tenant, &request)?
                            {
                                return Ok(existing);
                            }
                            let channels = state.tenant_delivery_channels(&tenant.tenant_id)?;
                            let ready = state.ready_tenants.get(&tenant.tenant_id);
                            let channel = select_tenant_channel(
                                &channels,
                                ready,
                                request.udt_type_script.as_ref(),
                            )?;
                            state.delivery_manager.accept_on_channel(
                                &registration,
                                &tenant,
                                channel,
                                request,
                                crate::now_timestamp_as_millis_u64(),
                            )
                        })()
                        .map(|delivery| {
                            let key = delivery.key();
                            LspServiceState::schedule_delivery_deadline(
                                &myself,
                                key,
                                delivery.buffer_deadline,
                            );
                            let _ = myself.send_message(LspServiceMessage::ResumeDelivery(key));
                            DeliveryDecision::Buffered
                        }),
                        Ok(None) => Err(format!(
                            "tenant {} is not registered",
                            registration.tenant_id
                        )),
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            LspServiceMessage::ResumeDelivery(key) => {
                state.reconciling_deliveries.remove(&key);
                state.cancel_scheduled_delivery_retry(&key);
                self.resume_delivery(myself, state, key).await?;
            }
            LspServiceMessage::ExpireDelivery(key) => {
                self.expire_delivery(myself, state, key).await?;
            }
            LspServiceMessage::TenantChannelOnline(tenant_pubkey, channel_id) => {
                if let Some(tenant) = state.registry.find_by_tenant_pubkey(&tenant_pubkey)? {
                    #[cfg(feature = "watchtower")]
                    {
                        let watch_result = super::watch::ensure_hosted_watch_channel(
                            &state.signing_events,
                            &state.watchtower_store,
                            &tenant,
                            channel_id,
                            &state.config.watchtower_quotas(),
                        );
                        state.refresh_tenant_pending_unwatched(&tenant);
                        if let Err(error) = watch_result {
                            let remove_tenant = if let Some(ready) =
                                state.ready_tenants.get_mut(&tenant.tenant_id)
                            {
                                ready.remove(&channel_id);
                                ready.is_empty()
                            } else {
                                false
                            };
                            if remove_tenant {
                                state.ready_tenants.remove(&tenant.tenant_id);
                            }
                            tracing::warn!(
                                tenant_id = %tenant.tenant_id,
                                %channel_id,
                                %error,
                                "Refusing to mark hosted channel online without watchtower monitoring"
                            );
                            return Ok(());
                        }
                    }
                    let tenant_store = state
                        .store
                        .namespaced(NodeNamespace::hosted_tenant(tenant.tenant_id.as_str()));
                    let asset = tenant_store
                        .get_channel_actor_state(&channel_id)
                        .and_then(|channel| channel.funding_udt_type_script.clone());
                    let tenant =
                        match state
                            .registry
                            .bind_channel(&tenant.tenant_id, channel_id, asset)
                        {
                            Ok(tenant) => tenant,
                            Err(error) => {
                                tracing::warn!(
                                    tenant_id = %tenant.tenant_id,
                                    channel_id = %channel_id,
                                    %error,
                                    "Ignoring hosted tenant channel with conflicting metadata"
                                );
                                return Ok(());
                            }
                        };
                    state
                        .ready_tenants
                        .entry(tenant.tenant_id.clone())
                        .or_default()
                        .insert(channel_id);
                    for delivery in state.delivery_manager.list_pending()? {
                        if delivery.tenant_id == tenant.tenant_id
                            && (delivery.private_channel_id == channel_id
                                || delivery.can_reselect_channel())
                        {
                            let _ = myself
                                .send_message(LspServiceMessage::ResumeDelivery(delivery.key()));
                        }
                    }
                }
            }
            LspServiceMessage::TenantChannelOffline(tenant_pubkey, channel_id) => {
                if let Some(tenant) = state.registry.find_by_tenant_pubkey(&tenant_pubkey)? {
                    let remove_tenant =
                        if let Some(ready) = state.ready_tenants.get_mut(&tenant.tenant_id) {
                            ready.remove(&channel_id);
                            ready.is_empty()
                        } else {
                            false
                        };
                    if remove_tenant {
                        state.ready_tenants.remove(&tenant.tenant_id);
                    }
                    #[cfg(feature = "watchtower")]
                    state.refresh_tenant_pending_unwatched(&tenant);
                }
            }
            LspServiceMessage::PaymentOutcomeReady(request, reply) => {
                let PaymentOutcomeRequest {
                    session_key,
                    payment_status,
                    failure,
                    failure_code,
                } = request;
                let result = match state.delivery_for_outcome(session_key) {
                    Ok(Some(delivery)) => {
                        let key = delivery.key();
                        let deadline = delivery.buffer_deadline;
                        let decision = state.record_payment_outcome(
                            &key,
                            payment_status,
                            failure,
                            failure_code,
                        );
                        match &decision {
                            Ok(PaymentOutcomeDecision::RetryDelivery) => {
                                state.schedule_delivery_retry(&myself, key, deadline);
                            }
                            Ok(PaymentOutcomeDecision::SettleUpstream) => {
                                state.cancel_scheduled_delivery_retry(&key);
                            }
                            Err(_) => {}
                        }
                        decision
                    }
                    Ok(_) => Ok(PaymentOutcomeDecision::SettleUpstream),
                    Err(error) => Err(error),
                };
                if matches!(&result, Ok(PaymentOutcomeDecision::SettleUpstream))
                    && payment_status.is_final()
                {
                    if let Ok(Some(delivery)) = state.delivery_for_outcome(session_key) {
                        state.schedule_reconciliation_retry(&myself, delivery.key());
                    }
                }
                let _ = reply.send(result);
            }
            LspServiceMessage::PaymentOutcomeSettled(outcome) => {
                let SettledPaymentOutcome {
                    session_key,
                    payment_status,
                    failure,
                } = outcome;
                if let Some(delivery) = state.delivery_for_outcome(session_key)? {
                    let key = delivery.key();
                    state.finish_upstream_settlement(&key, payment_status, failure)?;
                }
            }
            LspServiceMessage::ReconcileInFlightDeliveries => {
                for delivery in state.delivery_manager.list_pending()? {
                    let key = delivery.key();
                    if matches!(
                        delivery.status,
                        LspPaymentDeliveryStatus::InFlight
                            | LspPaymentDeliveryStatus::SettlingUpstream { .. }
                    ) && state.reconciling_deliveries.insert(key)
                    {
                        let _ = myself.send_message(LspServiceMessage::ResumeDelivery(key));
                    }
                }
            }
        }
        Ok(())
    }
}

impl LspService {
    const EXPIRATION_REASON: &'static str =
        "hosted tenant was unavailable before the buffer deadline";

    async fn fail_cancelled_invoice(
        myself: &ActorRef<LspServiceMessage>,
        state: &mut LspServiceState,
        delivery: &LspPaymentDelivery,
    ) -> Result<bool, String> {
        if !state.invoice_cancelled(delivery) {
            return Ok(false);
        }
        // Keep settlement recoverable across restarts. The final Failed state is
        // recorded only after Public T confirms upstream settlement was persisted.
        let reason = "hosted invoice was cancelled".to_string();
        let delivery = state.delivery_manager.transition_with_error(
            &delivery.key(),
            LspPaymentDeliveryStatus::SettlingUpstream {
                payment_status: PaymentStatus::Failed,
                failure: Some(reason.clone()),
            },
            Some((reason, Some(TlcErrorCode::InvoiceCancelled))),
            crate::now_timestamp_as_millis_u64(),
        )?;
        Self::finish_failed_delivery(myself, state, delivery).await?;
        Ok(true)
    }

    async fn inspect_upstream(
        state: &LspServiceState,
        delivery: &LspPaymentDelivery,
    ) -> Result<BufferedTrampolineUpstreamStatus, String> {
        ractor::call_t!(
            state.public_network_actor,
            |reply| NetworkActorMessage::new_command(
                FiberActorCommand::InspectBufferedTrampolineUpstream {
                    request: delivery.request.clone(),
                    reply,
                },
            ),
            5_000
        )
        .map_err(|error| error.to_string())
    }

    fn fail_removed_delivery(
        state: &mut LspServiceState,
        key: &LspPaymentDeliveryKey,
    ) -> Result<(), String> {
        state.delivery_manager.transition(
            key,
            LspPaymentDeliveryStatus::Failed {
                reason: "upstream TLC was removed before hosted delivery dispatch".to_string(),
            },
            crate::now_timestamp_as_millis_u64(),
        )?;
        state.cancel_scheduled_delivery_retry(key);
        state.reconciling_deliveries.remove(key);
        Ok(())
    }

    async fn finish_failed_delivery(
        myself: &ActorRef<LspServiceMessage>,
        state: &mut LspServiceState,
        delivery: LspPaymentDelivery,
    ) -> Result<(), String> {
        let LspPaymentDeliveryStatus::SettlingUpstream {
            payment_status: PaymentStatus::Failed,
            failure,
        } = &delivery.status
        else {
            return Err(format!(
                "hosted payment {} has no failed delivery to settle",
                delivery.payment_hash
            ));
        };
        let reason = failure
            .clone()
            .or_else(|| delivery.last_error.clone())
            .unwrap_or_else(|| "hosted payment failed".to_string());
        let error_code = delivery
            .last_error_code
            .unwrap_or(TlcErrorCode::PermanentNodeFailure);
        let failed = ractor::call_t!(
            state.public_network_actor,
            |reply| NetworkActorMessage::new_command(FiberActorCommand::FailBufferedTrampoline {
                request: delivery.request.clone(),
                reason: reason.clone(),
                error_code,
                reply,
            },),
            10_000
        )
        .map_err(|error| error.to_string())
        .and_then(|result| result);
        match failed {
            Ok(true) => {
                state.finish_upstream_settlement(
                    &delivery.key(),
                    PaymentStatus::Failed,
                    Some(reason),
                )?;
            }
            Ok(false) => {
                state.delivery_manager.transition(
                    &delivery.key(),
                    LspPaymentDeliveryStatus::InFlight,
                    crate::now_timestamp_as_millis_u64(),
                )?;
            }
            Err(error) => {
                tracing::warn!(
                    payment_hash = %delivery.payment_hash,
                    %error,
                    "Failed to settle hosted delivery failure"
                );
                state.schedule_reconciliation_retry(myself, delivery.key());
            }
        }
        Ok(())
    }

    async fn resume_delivery(
        &self,
        myself: ActorRef<LspServiceMessage>,
        state: &mut LspServiceState,
        key: LspPaymentDeliveryKey,
    ) -> Result<(), String> {
        let Some(mut delivery) = state.delivery_manager.get(&key)? else {
            return Ok(());
        };
        let payment_hash = delivery.payment_hash;
        if delivery.status.is_final() {
            return Ok(());
        }
        // A deferred delivery has not created its downstream payment session yet. Looking up the
        // hash at that point can find the upstream trampoline session and incorrectly treat it as
        // the downstream delivery. Only reconcile a session after dispatch has actually started.
        if matches!(
            delivery.status,
            LspPaymentDeliveryStatus::Dispatching
                | LspPaymentDeliveryStatus::InFlight
                | LspPaymentDeliveryStatus::SettlingUpstream { .. }
        ) {
            let existing_payment = ractor::call_t!(
                state.public_network_actor,
                |reply| NetworkActorMessage::new_command(FiberActorCommand::GetPayment(
                    delivery.request.hosted_session_key(),
                    reply,
                )),
                5_000
            );
            if let Ok(Ok(payment)) = existing_payment {
                if payment.status.is_final() {
                    let decision = state.record_payment_outcome(
                        &key,
                        payment.status,
                        payment.failed_error.clone(),
                        payment.failed_error_code,
                    )?;
                    if decision == PaymentOutcomeDecision::RetryDelivery {
                        state.schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
                        return Ok(());
                    }
                    let settlement = ractor::call_t!(
                        state.public_network_actor,
                        |reply| NetworkActorMessage::new_command(
                            FiberActorCommand::ReconcileBufferedTrampolineSettlement {
                                session_key: delivery.request.hosted_session_key(),
                                reply,
                            },
                        ),
                        10_000
                    )
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    if let Err(error) = settlement {
                        tracing::warn!(
                            %payment_hash,
                            %error,
                            "Failed to reconcile hosted upstream settlement"
                        );
                        state.schedule_reconciliation_retry(&myself, key);
                        return Ok(());
                    }
                    state.finish_upstream_settlement(&key, payment.status, payment.failed_error)?;
                } else {
                    let _ = state.record_payment_outcome(
                        &key,
                        payment.status,
                        payment.failed_error,
                        payment.failed_error_code,
                    )?;
                    state.schedule_reconciliation_retry(&myself, key);
                }
                return Ok(());
            }
            if matches!(
                delivery.status,
                LspPaymentDeliveryStatus::SettlingUpstream {
                    payment_status: PaymentStatus::Failed,
                    ..
                }
            ) && delivery.last_error_code.is_some()
            {
                return Self::finish_failed_delivery(&myself, state, delivery).await;
            }
            if matches!(
                delivery.status,
                LspPaymentDeliveryStatus::InFlight
                    | LspPaymentDeliveryStatus::SettlingUpstream { .. }
            ) {
                state.schedule_reconciliation_retry(&myself, key);
                return Ok(());
            }
        }

        match Self::inspect_upstream(state, &delivery).await {
            Ok(BufferedTrampolineUpstreamStatus::Pending) => {}
            Ok(BufferedTrampolineUpstreamStatus::Removed) => {
                Self::fail_removed_delivery(state, &key)?;
                return Ok(());
            }
            Ok(BufferedTrampolineUpstreamStatus::Unknown) => {
                tracing::warn!(
                    %payment_hash,
                    "Cannot determine hosted delivery upstream TLC state"
                );
                state.schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(
                    %payment_hash,
                    %error,
                    "Failed to inspect hosted delivery upstream TLC"
                );
                state.schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
                return Ok(());
            }
        }

        // Check before waiting for the tenant channel or the buffer deadline.
        if Self::fail_cancelled_invoice(&myself, state, &delivery).await? {
            return Ok(());
        }

        let now = crate::now_timestamp_as_millis_u64();
        if now >= delivery.buffer_deadline {
            let _ = myself.send_message(LspServiceMessage::ExpireDelivery(key));
            return Ok(());
        }

        let Some(tenant) = state.registry.get(&delivery.tenant_id)? else {
            let _ = myself.send_message(LspServiceMessage::ExpireDelivery(key));
            return Ok(());
        };
        let ready = state.ready_tenants.get(&tenant.tenant_id);
        let channel_ready = if delivery.can_reselect_channel() {
            let channels = state.tenant_delivery_channels(&tenant.tenant_id)?;
            match select_tenant_channel(&channels, ready, delivery.request.udt_type_script.as_ref())
            {
                Ok(channel) if ready.is_some_and(|ready| ready.contains(&channel.channel_id)) => {
                    delivery = state
                        .delivery_manager
                        .reselect_channel(&key, channel, now)?;
                    true
                }
                _ => false,
            }
        } else {
            ready.is_some_and(|ready| ready.contains(&delivery.private_channel_id))
        };
        if !channel_ready {
            state.schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
            return Ok(());
        }
        if let Err(error) = state.supervisor.ensure(&tenant).await {
            tracing::debug!(
                "Deferred hosted delivery {} while tenant {} starts: {}",
                payment_hash,
                delivery.tenant_id,
                error
            );
            state.schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
            return Ok(());
        }

        // Cancellation may have been persisted while cold tenant activation was awaited.
        if Self::fail_cancelled_invoice(&myself, state, &delivery).await? {
            return Ok(());
        }

        delivery = state.delivery_manager.begin_dispatch(&key, now)?;
        let dispatch_result = match ractor::call_t!(
            state.public_network_actor,
            |reply| NetworkActorMessage::new_command(
                FiberActorCommand::DispatchBufferedTrampoline {
                    request: delivery.request.clone(),
                    reply,
                },
            ),
            10_000
        ) {
            Ok(result) => result,
            Err(error) => Err(DeliveryDispatchError::Temporary {
                reason: format!("failed to call Public T for hosted payment dispatch: {error}"),
            }),
        };
        match dispatch_result {
            Ok(()) => {
                state.delivery_manager.transition(
                    &key,
                    LspPaymentDeliveryStatus::InFlight,
                    crate::now_timestamp_as_millis_u64(),
                )?;
            }
            Err(DeliveryDispatchError::Temporary { reason }) => {
                state.delivery_manager.transition_with_error(
                    &key,
                    LspPaymentDeliveryStatus::Deferred,
                    Some((reason.clone(), Some(TlcErrorCode::TemporaryNodeFailure))),
                    crate::now_timestamp_as_millis_u64(),
                )?;
                tracing::debug!(
                    %payment_hash,
                    tenant_id = %delivery.tenant_id,
                    error = %reason,
                    attempt_count = delivery.attempt_count,
                    "Deferring hosted delivery after a transient dispatch failure"
                );
                state.schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
            }
            Err(DeliveryDispatchError::Permanent { reason, error_code }) => {
                let delivery = state.delivery_manager.transition_with_error(
                    &key,
                    LspPaymentDeliveryStatus::SettlingUpstream {
                        payment_status: PaymentStatus::Failed,
                        failure: Some(reason.clone()),
                    },
                    Some((reason, Some(error_code))),
                    crate::now_timestamp_as_millis_u64(),
                )?;
                Self::finish_failed_delivery(&myself, state, delivery).await?;
            }
        }
        Ok(())
    }

    async fn expire_delivery(
        &self,
        myself: ActorRef<LspServiceMessage>,
        state: &mut LspServiceState,
        key: LspPaymentDeliveryKey,
    ) -> Result<(), String> {
        let Some(delivery) = state.delivery_manager.get(&key)? else {
            return Ok(());
        };
        let payment_hash = delivery.payment_hash;
        if matches!(delivery.status, LspPaymentDeliveryStatus::InFlight) {
            return Ok(());
        }
        if delivery.status.is_final() {
            state.cancel_scheduled_delivery_retry(&key);
            return Ok(());
        }
        if matches!(
            delivery.status,
            LspPaymentDeliveryStatus::SettlingUpstream { .. }
        ) {
            state.cancel_scheduled_delivery_retry(&key);
            state.schedule_reconciliation_retry(&myself, key);
            return Ok(());
        }
        let now = crate::now_timestamp_as_millis_u64();
        if now < delivery.buffer_deadline {
            LspServiceState::schedule_delivery_deadline(&myself, key, delivery.buffer_deadline);
            return Ok(());
        }
        match Self::inspect_upstream(state, &delivery).await {
            Ok(BufferedTrampolineUpstreamStatus::Pending) => {}
            Ok(BufferedTrampolineUpstreamStatus::Removed) => {
                Self::fail_removed_delivery(state, &key)?;
                return Ok(());
            }
            Ok(BufferedTrampolineUpstreamStatus::Unknown) => {
                tracing::warn!(
                    %payment_hash,
                    "Cannot expire hosted delivery while upstream TLC state is unknown"
                );
                state.schedule_reconciliation_retry(&myself, key);
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(
                    %payment_hash,
                    %error,
                    "Failed to inspect upstream TLC before expiring hosted delivery"
                );
                state.schedule_reconciliation_retry(&myself, key);
                return Ok(());
            }
        }
        let reason = Self::EXPIRATION_REASON.to_string();
        state.cancel_scheduled_delivery_retry(&key);
        let delivery = state.delivery_manager.transition_with_error(
            &key,
            LspPaymentDeliveryStatus::SettlingUpstream {
                payment_status: PaymentStatus::Failed,
                failure: Some(reason.clone()),
            },
            Some((reason, Some(TlcErrorCode::TemporaryNodeFailure))),
            now,
        )?;
        Self::finish_failed_delivery(&myself, state, delivery).await
    }
}
