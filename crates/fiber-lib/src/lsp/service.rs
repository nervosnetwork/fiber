use std::{collections::HashMap, sync::Arc, time::Duration};

use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort};

use crate::fiber::network::{
    BufferedTrampolineUpstreamStatus, NetworkActorCommand, NetworkActorMessage,
};
use crate::fiber_types::{Hash256, PaymentStatus, Privkey, Pubkey, TlcErrorCode};
use crate::invoice::CkbInvoice;
use crate::store::Store;

use super::{
    is_permanent_hosted_payment_failure, HostedTenantRpcContext, HostedTenantStatus, LspConfig,
    LspInvoiceRegistration, LspInvoiceRegistry, LspPaymentDelivery, LspPaymentDeliveryKey,
    LspPaymentDeliveryLimits, LspPaymentDeliveryManager, LspPaymentDeliveryStatus,
    LspPaymentDispatchError, LspPaymentOutcomeDecision, TenantId, TenantRegistry,
    TenantRuntimeFactory, TenantRuntimeStatus, TenantSupervisor, TrampolineForwardingRequest,
};

/// Runtime dependencies of the LSP service container.
pub struct LspServiceArgs {
    pub config: LspConfig,
    pub public_node_id: Pubkey,
    pub public_network_actor: ActorRef<NetworkActorMessage>,
    pub store: Store,
    pub runtime_factory: Arc<dyn TenantRuntimeFactory>,
    pub signing_key: Privkey,
}

/// Read-only status for callers that need to discover the hosted service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspServiceStatus {
    pub public_node_id: Pubkey,
    pub tenant_store_root: std::path::PathBuf,
    pub registered_tenants: usize,
    pub active_tenants: usize,
}

/// Result of the idempotent hosted tenant registration operation.
pub struct HostedTenantRegistration {
    pub status: HostedTenantStatus,
    pub created: bool,
}

/// Result of consulting the hosted invoice registry for an incoming trampoline TLC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LspDeliveryDecision {
    NotHosted,
    Buffered,
}

/// Commands accepted by the LSP service container.
pub enum LspServiceMessage {
    GetStatus(RpcReplyPort<LspServiceStatus>),
    RegisterTenant(
        TenantId,
        RpcReplyPort<Result<HostedTenantRegistration, String>>,
    ),
    EnsureTenant(TenantId, RpcReplyPort<Result<HostedTenantStatus, String>>),
    EvictTenant(TenantId, RpcReplyPort<Result<HostedTenantStatus, String>>),
    ListTenants(RpcReplyPort<Result<Vec<HostedTenantStatus>, String>>),
    /// Returns an active tenant's scoped RPC backend, starting it when needed.
    GetTenantRpcContext(
        TenantId,
        RpcReplyPort<Result<HostedTenantRpcContext, String>>,
    ),
    RegisterInvoice {
        tenant_id: TenantId,
        invoice: CkbInvoice,
        buffer_duration_ms: Option<u64>,
        reply: RpcReplyPort<Result<LspInvoiceRegistration, String>>,
    },
    GetPaymentDelivery(
        Hash256,
        RpcReplyPort<Result<Option<LspPaymentDelivery>, String>>,
    ),
    AcceptTrampolineDelivery(
        TrampolineForwardingRequest,
        RpcReplyPort<Result<LspDeliveryDecision, String>>,
    ),
    ResumeDelivery(LspPaymentDeliveryKey),
    ExpireDelivery(LspPaymentDeliveryKey),
    TenantChannelOnline(Pubkey, Hash256),
    TenantChannelOffline(Pubkey, Hash256),
    PaymentOutcomeReady {
        payment_hash: Hash256,
        payment_status: PaymentStatus,
        failure: Option<String>,
        failure_code: Option<TlcErrorCode>,
        reply: RpcReplyPort<Result<LspPaymentOutcomeDecision, String>>,
    },
    PaymentOutcomeSettled {
        payment_hash: Hash256,
        payment_status: PaymentStatus,
        failure: Option<String>,
    },
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
    pub ready_tenants: HashMap<TenantId, Hash256>,
}

impl LspServiceState {
    fn tenant_status(&self, record: crate::lsp::HostedTenantRecord) -> HostedTenantStatus {
        let runtime_status = if self.supervisor.is_active(&record.tenant_id) {
            TenantRuntimeStatus::Active
        } else {
            TenantRuntimeStatus::Cold
        };
        HostedTenantStatus {
            channel_online: self.ready_tenants.contains_key(&record.tenant_id),
            record,
            runtime_status,
        }
    }

    fn get_tenant_status(&self, tenant_id: &TenantId) -> Result<HostedTenantStatus, String> {
        self.registry
            .get(tenant_id)?
            .map(|record| self.tenant_status(record))
            .ok_or_else(|| format!("tenant {tenant_id} is not registered"))
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

    fn schedule_delivery_retry(
        myself: &ActorRef<LspServiceMessage>,
        key: LspPaymentDeliveryKey,
        deadline: u64,
    ) {
        let remaining = deadline.saturating_sub(crate::now_timestamp_as_millis_u64());
        if remaining == 0 {
            let _ = myself.send_message(LspServiceMessage::ExpireDelivery(key));
        } else {
            myself.send_after(Duration::from_millis(remaining.min(1_000)), move || {
                LspServiceMessage::ResumeDelivery(key)
            });
        }
    }

    fn schedule_reconciliation_retry(
        myself: &ActorRef<LspServiceMessage>,
        key: LspPaymentDeliveryKey,
    ) {
        myself.send_after(Duration::from_secs(1), move || {
            LspServiceMessage::ResumeDelivery(key)
        });
    }

    fn record_payment_outcome(
        &self,
        key: &LspPaymentDeliveryKey,
        status: PaymentStatus,
        failure: Option<String>,
        failure_code: Option<TlcErrorCode>,
    ) -> Result<LspPaymentOutcomeDecision, String> {
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
            return Ok(LspPaymentOutcomeDecision::RetryDelivery);
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
        Ok(LspPaymentOutcomeDecision::SettleUpstream)
    }

    fn finish_upstream_settlement(
        &self,
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
        let supervisor =
            TenantSupervisor::new(args.runtime_factory.clone(), args.config.max_active_tenants);
        for tenant in &args.config.tenants {
            let tenant_id = TenantId::new(tenant.clone()).map_err(anyhow::Error::msg)?;
            let record = supervisor
                .provision(&tenant_id)
                .map_err(anyhow::Error::msg)?;
            registry.register(record).map_err(anyhow::Error::msg)?;
        }
        for delivery in delivery_manager
            .list_pending()
            .map_err(anyhow::Error::msg)?
        {
            let key = delivery.key();
            let _ = myself.send_message(LspServiceMessage::ResumeDelivery(key));
            LspServiceState::schedule_delivery_deadline(&myself, key, delivery.buffer_deadline);
        }
        Ok(LspServiceState {
            config: args.config,
            public_node_id: args.public_node_id,
            public_network_actor: args.public_network_actor,
            store: args.store,
            registry,
            invoice_registry,
            delivery_manager,
            supervisor,
            signing_key: args.signing_key,
            ready_tenants: HashMap::new(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            LspServiceMessage::GetStatus(reply) => {
                let registered_tenants = state.registry.list().map_or(0, |tenants| tenants.len());
                let _ = reply.send(LspServiceStatus {
                    public_node_id: state.public_node_id,
                    tenant_store_root: state.config.tenant_store_root(),
                    registered_tenants,
                    active_tenants: state.supervisor.active_count(),
                });
            }
            LspServiceMessage::RegisterTenant(tenant_id, reply) => {
                let result = state.registry.get(&tenant_id).and_then(|existing| {
                    let created = existing.is_none();
                    state
                        .supervisor
                        .provision(&tenant_id)
                        .and_then(|record| state.registry.register(record))
                        .map(|record| HostedTenantRegistration {
                            status: state.tenant_status(record),
                            created,
                        })
                });
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
            LspServiceMessage::GetPaymentDelivery(payment_hash, reply) => {
                let _ = reply.send(state.delivery_manager.get_by_payment_hash(&payment_hash));
            }
            LspServiceMessage::AcceptTrampolineDelivery(request, reply) => {
                let result = match state.invoice_registry.get(&request.payment_hash) {
                    Ok(None) => Ok(LspDeliveryDecision::NotHosted),
                    Ok(Some(registration)) => match state.registry.get(&registration.tenant_id) {
                        Ok(Some(_)) if registration.hint.payload.buffer_duration_ms == 0 => {
                            Ok(LspDeliveryDecision::NotHosted)
                        }
                        Ok(Some(tenant)) => state
                            .delivery_manager
                            .accept(
                                &registration,
                                &tenant,
                                request,
                                crate::now_timestamp_as_millis_u64(),
                            )
                            .map(|delivery| {
                                let key = delivery.key();
                                LspServiceState::schedule_delivery_deadline(
                                    &myself,
                                    key,
                                    delivery.buffer_deadline,
                                );
                                let _ = myself.send_message(LspServiceMessage::ResumeDelivery(key));
                                LspDeliveryDecision::Buffered
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
                self.resume_delivery(myself, state, key).await?;
            }
            LspServiceMessage::ExpireDelivery(key) => {
                self.expire_delivery(myself, state, key).await?;
            }
            LspServiceMessage::TenantChannelOnline(invoice_pubkey, channel_id) => {
                if let Some(tenant) = state.registry.find_by_invoice_pubkey(&invoice_pubkey)? {
                    let tenant = match state
                        .registry
                        .bind_private_channel(&tenant.tenant_id, channel_id)
                    {
                        Ok(tenant) => tenant,
                        Err(error) => {
                            tracing::warn!(
                                tenant_id = %tenant.tenant_id,
                                channel_id = %channel_id,
                                %error,
                                "Ignoring an additional hosted tenant private channel"
                            );
                            return Ok(());
                        }
                    };
                    state
                        .ready_tenants
                        .insert(tenant.tenant_id.clone(), channel_id);
                    for delivery in state.delivery_manager.list_pending()? {
                        if delivery.tenant_id == tenant.tenant_id
                            && delivery.private_channel_id == channel_id
                        {
                            let _ = myself
                                .send_message(LspServiceMessage::ResumeDelivery(delivery.key()));
                        }
                    }
                }
            }
            LspServiceMessage::TenantChannelOffline(invoice_pubkey, channel_id) => {
                if let Some(tenant) = state.registry.find_by_invoice_pubkey(&invoice_pubkey)? {
                    if state.ready_tenants.get(&tenant.tenant_id) == Some(&channel_id) {
                        state.ready_tenants.remove(&tenant.tenant_id);
                    }
                }
            }
            LspServiceMessage::PaymentOutcomeReady {
                payment_hash,
                payment_status,
                failure,
                failure_code,
                reply,
            } => {
                let result = match state
                    .delivery_manager
                    .get_active_by_payment_hash(&payment_hash)
                {
                    Ok(Some(delivery)) if !delivery.status.is_final() => {
                        let key = delivery.key();
                        let deadline = delivery.buffer_deadline;
                        let decision = state.record_payment_outcome(
                            &key,
                            payment_status,
                            failure,
                            failure_code,
                        );
                        if let Ok(LspPaymentOutcomeDecision::RetryDelivery) = decision {
                            LspServiceState::schedule_delivery_retry(&myself, key, deadline);
                        }
                        decision
                    }
                    Ok(_) => Ok(LspPaymentOutcomeDecision::SettleUpstream),
                    Err(error) => Err(error),
                };
                if matches!(&result, Ok(LspPaymentOutcomeDecision::SettleUpstream))
                    && payment_status.is_final()
                {
                    if let Ok(Some(delivery)) = state
                        .delivery_manager
                        .get_active_by_payment_hash(&payment_hash)
                    {
                        LspServiceState::schedule_reconciliation_retry(&myself, delivery.key());
                    }
                }
                let _ = reply.send(result);
            }
            LspServiceMessage::PaymentOutcomeSettled {
                payment_hash,
                payment_status,
                failure,
            } => {
                if let Some(delivery) = state
                    .delivery_manager
                    .get_active_by_payment_hash(&payment_hash)?
                {
                    let key = delivery.key();
                    match delivery.status {
                        LspPaymentDeliveryStatus::ExpiringUpstream { reason } => {
                            match payment_status {
                                PaymentStatus::Failed => {
                                    state.delivery_manager.transition(
                                        &key,
                                        LspPaymentDeliveryStatus::Expired { reason },
                                        crate::now_timestamp_as_millis_u64(),
                                    )?;
                                }
                                PaymentStatus::Success => {
                                    state.delivery_manager.transition(
                                        &key,
                                        LspPaymentDeliveryStatus::InFlight,
                                        crate::now_timestamp_as_millis_u64(),
                                    )?;
                                    let _ = state.record_payment_outcome(
                                        &key,
                                        payment_status,
                                        failure.clone(),
                                        None,
                                    )?;
                                    state.finish_upstream_settlement(
                                        &key,
                                        payment_status,
                                        failure,
                                    )?;
                                }
                                PaymentStatus::Created | PaymentStatus::Inflight => {
                                    return Err(anyhow::Error::msg(format!(
                                        "hosted payment {payment_hash} received a non-final settled outcome"
                                    ))
                                    .into());
                                }
                            }
                        }
                        status if !status.is_final() => {
                            state.finish_upstream_settlement(&key, payment_status, failure)?;
                        }
                        _ => {}
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

    async fn inspect_upstream(
        state: &LspServiceState,
        delivery: &LspPaymentDelivery,
    ) -> Result<BufferedTrampolineUpstreamStatus, String> {
        ractor::call_t!(
            state.public_network_actor,
            |reply| NetworkActorMessage::new_command(
                NetworkActorCommand::InspectBufferedTrampolineUpstream {
                    request: delivery.request.clone(),
                    reply,
                },
            ),
            5_000
        )
        .map_err(|error| error.to_string())
    }

    fn cancel_delivery(state: &LspServiceState, key: &LspPaymentDeliveryKey) -> Result<(), String> {
        state.delivery_manager.transition(
            key,
            LspPaymentDeliveryStatus::Cancelled {
                reason: "upstream TLC was removed before hosted delivery dispatch".to_string(),
            },
            crate::now_timestamp_as_millis_u64(),
        )?;
        Ok(())
    }

    async fn finish_expiration(
        myself: &ActorRef<LspServiceMessage>,
        state: &mut LspServiceState,
        delivery: LspPaymentDelivery,
    ) -> Result<(), String> {
        let LspPaymentDeliveryStatus::ExpiringUpstream { reason } = &delivery.status else {
            return Err(format!(
                "hosted payment {} is not expiring upstream",
                delivery.payment_hash
            ));
        };
        let failed = ractor::call_t!(
            state.public_network_actor,
            |reply| NetworkActorMessage::new_command(NetworkActorCommand::FailBufferedTrampoline {
                request: delivery.request.clone(),
                reason: reason.clone(),
                error_code: TlcErrorCode::TemporaryNodeFailure,
                reply,
            },),
            10_000
        )
        .map_err(|error| error.to_string())
        .and_then(|result| result);
        let failed = match failed {
            Ok(failed) => failed,
            Err(error) => {
                tracing::warn!(
                    payment_hash = %delivery.payment_hash,
                    %error,
                    "Failed to settle expired hosted delivery"
                );
                LspServiceState::schedule_reconciliation_retry(myself, delivery.key());
                return Ok(());
            }
        };
        let status = if failed {
            LspPaymentDeliveryStatus::Expired {
                reason: reason.clone(),
            }
        } else {
            LspPaymentDeliveryStatus::InFlight
        };
        state.delivery_manager.transition(
            &delivery.key(),
            status,
            crate::now_timestamp_as_millis_u64(),
        )?;
        Ok(())
    }

    async fn finish_permanent_dispatch_failure(
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
                "hosted payment {} has no permanent dispatch failure to settle",
                delivery.payment_hash
            ));
        };
        let reason = failure
            .clone()
            .or_else(|| delivery.last_error.clone())
            .unwrap_or_else(|| "hosted payment dispatch failed permanently".to_string());
        let error_code = delivery
            .last_error_code
            .unwrap_or(TlcErrorCode::PermanentNodeFailure);
        let failed = ractor::call_t!(
            state.public_network_actor,
            |reply| NetworkActorMessage::new_command(NetworkActorCommand::FailBufferedTrampoline {
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
                    "Failed to settle permanent hosted dispatch failure"
                );
                LspServiceState::schedule_reconciliation_retry(myself, delivery.key());
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
        if matches!(
            delivery.status,
            LspPaymentDeliveryStatus::ExpiringUpstream { .. }
        ) {
            return Self::finish_expiration(&myself, state, delivery).await;
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
                |reply| NetworkActorMessage::new_command(NetworkActorCommand::GetPayment(
                    payment_hash,
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
                    if decision == LspPaymentOutcomeDecision::RetryDelivery {
                        LspServiceState::schedule_delivery_retry(
                            &myself,
                            key,
                            delivery.buffer_deadline,
                        );
                        return Ok(());
                    }
                    let settlement = ractor::call_t!(
                        state.public_network_actor,
                        |reply| NetworkActorMessage::new_command(
                            NetworkActorCommand::ReconcileBufferedTrampolineSettlement {
                                payment_hash,
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
                        LspServiceState::schedule_reconciliation_retry(&myself, key);
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
                    LspServiceState::schedule_reconciliation_retry(&myself, key);
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
                return Self::finish_permanent_dispatch_failure(&myself, state, delivery).await;
            }
            if matches!(
                delivery.status,
                LspPaymentDeliveryStatus::InFlight
                    | LspPaymentDeliveryStatus::SettlingUpstream { .. }
            ) {
                LspServiceState::schedule_reconciliation_retry(&myself, key);
                return Ok(());
            }
        }

        match Self::inspect_upstream(state, &delivery).await {
            Ok(BufferedTrampolineUpstreamStatus::Pending) => {}
            Ok(BufferedTrampolineUpstreamStatus::Removed) => {
                Self::cancel_delivery(state, &key)?;
                return Ok(());
            }
            Ok(BufferedTrampolineUpstreamStatus::Unknown) => {
                tracing::warn!(
                    %payment_hash,
                    "Cannot determine hosted delivery upstream TLC state"
                );
                LspServiceState::schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(
                    %payment_hash,
                    %error,
                    "Failed to inspect hosted delivery upstream TLC"
                );
                LspServiceState::schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
                return Ok(());
            }
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
        let channel_ready =
            state.ready_tenants.get(&tenant.tenant_id) == Some(&delivery.private_channel_id);
        if !channel_ready {
            LspServiceState::schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
            return Ok(());
        }
        if let Err(error) = state.supervisor.ensure(&tenant).await {
            tracing::debug!(
                "Deferred hosted delivery {} while tenant {} starts: {}",
                payment_hash,
                delivery.tenant_id,
                error
            );
            LspServiceState::schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
            return Ok(());
        }

        delivery = state.delivery_manager.begin_dispatch(&key, now)?;
        let dispatch_result = match ractor::call_t!(
            state.public_network_actor,
            |reply| NetworkActorMessage::new_command(
                NetworkActorCommand::DispatchBufferedTrampoline {
                    request: delivery.request.clone(),
                    reply,
                },
            ),
            10_000
        ) {
            Ok(result) => result,
            Err(error) => Err(LspPaymentDispatchError::Temporary {
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
            Err(LspPaymentDispatchError::Temporary { reason }) => {
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
                LspServiceState::schedule_delivery_retry(&myself, key, delivery.buffer_deadline);
            }
            Err(LspPaymentDispatchError::Permanent { reason, error_code }) => {
                let delivery = state.delivery_manager.transition_with_error(
                    &key,
                    LspPaymentDeliveryStatus::SettlingUpstream {
                        payment_status: PaymentStatus::Failed,
                        failure: Some(reason.clone()),
                    },
                    Some((reason, Some(error_code))),
                    crate::now_timestamp_as_millis_u64(),
                )?;
                Self::finish_permanent_dispatch_failure(&myself, state, delivery).await?;
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
            return Ok(());
        }
        if matches!(
            delivery.status,
            LspPaymentDeliveryStatus::ExpiringUpstream { .. }
        ) {
            return Self::finish_expiration(&myself, state, delivery).await;
        }
        if matches!(
            delivery.status,
            LspPaymentDeliveryStatus::SettlingUpstream { .. }
        ) {
            LspServiceState::schedule_reconciliation_retry(&myself, key);
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
                Self::cancel_delivery(state, &key)?;
                return Ok(());
            }
            Ok(BufferedTrampolineUpstreamStatus::Unknown) => {
                tracing::warn!(
                    %payment_hash,
                    "Cannot expire hosted delivery while upstream TLC state is unknown"
                );
                LspServiceState::schedule_reconciliation_retry(&myself, key);
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(
                    %payment_hash,
                    %error,
                    "Failed to inspect upstream TLC before expiring hosted delivery"
                );
                LspServiceState::schedule_reconciliation_retry(&myself, key);
                return Ok(());
            }
        }
        let delivery = state.delivery_manager.transition(
            &key,
            LspPaymentDeliveryStatus::ExpiringUpstream {
                reason: Self::EXPIRATION_REASON.to_string(),
            },
            now,
        )?;
        Self::finish_expiration(&myself, state, delivery).await
    }
}
