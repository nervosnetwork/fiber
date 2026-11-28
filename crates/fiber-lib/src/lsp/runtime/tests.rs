use super::*;

impl HostedTenantRuntime {
    pub(crate) fn with_rpc_context(mut self, rpc_context: HostedTenantRpcContext) -> Self {
        self.rpc_context = Some(rpc_context);
        self
    }
}

impl HostedTenantRuntime {
    pub(crate) fn network_backed(
        tenant_pubkey: Pubkey,
        network_actor: ActorRef<NetworkActorMessage>,
    ) -> Self {
        Self {
            tenant_pubkey,
            runtime_actor: FiberActorRef::from_network(&network_actor),
            public_network_actor: None,
            rpc_context: None,
        }
    }
}
