use std::{collections::HashMap, sync::Arc};

use ractor::{Actor, ActorProcessingErr, ActorRef, ActorStatus};

use crate::fiber::{
    network::{NetworkActorEvent, NetworkActorMessage},
    types::FiberMessage,
};
use crate::fiber_types::{Hash256, Pubkey};

use super::TenantId;

#[derive(Clone)]
struct TenantRoute {
    invoice_pubkey: Pubkey,
    runtime_actor: ActorRef<NetworkActorMessage>,
}

#[derive(Default)]
struct TenantDispatcherState {
    runtimes: HashMap<TenantId, TenantRoute>,
    channels: HashMap<(TenantId, Hash256), ActorRef<NetworkActorMessage>>,
}

/// Routes co-located Fiber messages through a tenant and channel scoped
/// boundary instead of exposing tenant runtime actors as transport peers.
#[derive(Clone, Default)]
pub(crate) struct TenantMessageDispatcher {
    state: Arc<std::sync::RwLock<TenantDispatcherState>>,
}

impl TenantMessageDispatcher {
    pub(crate) fn register_runtime(
        &self,
        tenant_id: TenantId,
        invoice_pubkey: Pubkey,
        runtime_actor: ActorRef<NetworkActorMessage>,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "hosted tenant dispatcher lock is poisoned".to_string())?;
        if state.runtimes.iter().any(|(registered_id, route)| {
            registered_id != &tenant_id && route.invoice_pubkey == invoice_pubkey
        }) {
            return Err(format!(
                "hosted tenant invoice key {invoice_pubkey:?} is already registered"
            ));
        }
        if state.runtimes.get(&tenant_id).is_some_and(|route| {
            route.runtime_actor != runtime_actor
                && route.runtime_actor.get_status() < ActorStatus::Stopping
        }) {
            return Err(format!(
                "hosted tenant {tenant_id} is already owned by another runtime"
            ));
        }
        state
            .channels
            .retain(|(registered_id, _), _| registered_id != &tenant_id);
        state.runtimes.insert(
            tenant_id,
            TenantRoute {
                invoice_pubkey,
                runtime_actor,
            },
        );
        Ok(())
    }

    pub(crate) fn unregister_runtime(
        &self,
        tenant_id: &TenantId,
        runtime_actor: &ActorRef<NetworkActorMessage>,
    ) {
        if let Ok(mut state) = self.state.write() {
            if state
                .runtimes
                .get(tenant_id)
                .is_some_and(|route| &route.runtime_actor == runtime_actor)
            {
                state.runtimes.remove(tenant_id);
                state
                    .channels
                    .retain(|(registered_id, _), _| registered_id != tenant_id);
            }
        }
    }

    fn channel_id(message: &FiberMessage) -> Option<Hash256> {
        match message {
            FiberMessage::Init(_) => None,
            FiberMessage::ChannelInitialization(message) => Some(message.channel_id),
            FiberMessage::ChannelNormalOperation(message) => Some(message.get_channel_id()),
        }
    }

    fn route_to_tenant(
        &self,
        tenant_id: &TenantId,
        public_node_id: Pubkey,
        message: FiberMessage,
    ) -> Result<(), String> {
        let runtime_actor = {
            let mut state = self
                .state
                .write()
                .map_err(|_| "hosted tenant dispatcher lock is poisoned".to_string())?;
            let runtime_actor = state
                .runtimes
                .get(tenant_id)
                .map(|route| route.runtime_actor.clone())
                .ok_or_else(|| format!("hosted tenant {tenant_id} runtime is not registered"))?;
            if let Some(channel_id) = Self::channel_id(&message) {
                state
                    .channels
                    .entry((tenant_id.clone(), channel_id))
                    .or_insert_with(|| runtime_actor.clone())
                    .clone()
            } else {
                runtime_actor
            }
        };
        runtime_actor
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::FiberMessage(public_node_id, message, None),
            ))
            .map_err(|error| format!("failed to dispatch message to tenant {tenant_id}: {error}"))
    }

    fn route_to_public(
        &self,
        tenant_id: &TenantId,
        invoice_pubkey: Pubkey,
        public_network_actor: &ActorRef<NetworkActorMessage>,
        message: FiberMessage,
    ) -> Result<(), String> {
        {
            let mut state = self
                .state
                .write()
                .map_err(|_| "hosted tenant dispatcher lock is poisoned".to_string())?;
            let runtime_actor = state
                .runtimes
                .get_mut(tenant_id)
                .ok_or_else(|| format!("hosted tenant {tenant_id} runtime is not registered"))?;
            if runtime_actor.invoice_pubkey != invoice_pubkey {
                return Err(format!(
                    "hosted tenant {tenant_id} message source does not match its invoice key"
                ));
            }
            let runtime_actor = runtime_actor.runtime_actor.clone();
            if let Some(channel_id) = Self::channel_id(&message) {
                state
                    .channels
                    .insert((tenant_id.clone(), channel_id), runtime_actor);
            }
        }
        public_network_actor
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::FiberMessage(invoice_pubkey, message, None),
            ))
            .map_err(|error| {
                format!("failed to dispatch message from tenant {tenant_id} to Public T: {error}")
            })
    }

    #[cfg(test)]
    pub(crate) fn owns_channel(&self, tenant_id: &TenantId, channel_id: &Hash256) -> bool {
        self.state
            .read()
            .ok()
            .map(|state| {
                state
                    .channels
                    .contains_key(&(tenant_id.clone(), *channel_id))
            })
            .unwrap_or(false)
    }
}

pub(crate) struct HostedTenantEndpointArgs {
    pub tenant_id: TenantId,
    pub invoice_pubkey: Pubkey,
    pub public_node_id: Pubkey,
    pub public_network_actor: ActorRef<NetworkActorMessage>,
    pub dispatcher: TenantMessageDispatcher,
}

pub(crate) struct HostedTenantEndpoint;

#[async_trait::async_trait]
impl Actor for HostedTenantEndpoint {
    type Msg = NetworkActorMessage;
    type State = HostedTenantEndpointArgs;
    type Arguments = HostedTenantEndpointArgs;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(args)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let NetworkActorMessage::Event(NetworkActorEvent::FiberMessage(source, message, _)) =
            message
        else {
            tracing::warn!(tenant_id = %state.tenant_id, "Ignoring non-Fiber message sent to hosted tenant endpoint");
            return Ok(());
        };
        let result = if source == state.public_node_id {
            state
                .dispatcher
                .route_to_tenant(&state.tenant_id, state.public_node_id, message)
        } else if source == state.invoice_pubkey {
            state.dispatcher.route_to_public(
                &state.tenant_id,
                state.invoice_pubkey,
                &state.public_network_actor,
                message,
            )
        } else {
            Err(format!(
                "hosted tenant {} endpoint rejected unexpected source {source:?}",
                state.tenant_id
            ))
        };
        if let Err(error) = result {
            tracing::warn!(tenant_id = %state.tenant_id, %error, "Failed to dispatch hosted tenant Fiber message");
        }
        Ok(())
    }
}
