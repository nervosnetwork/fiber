#![cfg_attr(target_arch = "wasm32", allow(dead_code))] // wasm has no request scoping
//! The only way an RPC handler can reach an actor and a store.
//!
//! The wasm build has no request scoping (no tenants, no `Extensions`), so the
//! tenant half of this module is unused there.
//!
//! Every handler used to hold `actor` / `store` fields and each one re-derived
//! "am I running for a tenant?" with a copy-pasted prologue. Forgetting that
//! prologue meant a tenant token silently executed against the public node.
//!
//! A handler now owns a [`BackendResolver`] and gets a [`ScopedBackend`] from
//! [`BackendResolver::resolve`]. The resolver's fields are private to this
//! module, so no handler module can reach the public actor/store on its own, and
//! a [`ScopedBackend`] cannot be constructed outside this module either. The
//! tenant decision therefore happens exactly once per request, inside `resolve`.

use jsonrpsee::types::ErrorObjectOwned;

use crate::fiber::network::FiberActorRef;
use crate::fiber_types::{Pubkey, TenantId};
use crate::rpc::tenant::{resolve_tenant_rpc_context, HostedTenantRpcContext, LspActorHandle};
use crate::store::Store;

/// The identity a request is allowed to see while it runs for a tenant.
#[derive(Clone, Debug)]
pub(crate) struct TenantScope {
    pub(crate) tenant_id: TenantId,
    pub(crate) public_node_id: Pubkey,
    /// The tenant runtime's Fiber config, for handlers that rebuild a service
    /// handle (invoice keys, currency, node features).
    pub(crate) config: crate::FiberConfig,
}

/// Lets a public store answer a tenant request with that tenant's namespace.
///
/// Only the real store implements this: a test double has no tenant namespaces,
/// and tenant request paths are covered by the LSP integration tests.
pub trait RpcBackendStore: Clone {
    fn for_tenant(&self, store: Store) -> Self;
}

impl RpcBackendStore for Store {
    fn for_tenant(&self, store: Store) -> Self {
        store
    }
}

/// Actor and store one RPC handler actually runs against.
///
/// Fields are readable so the handler bodies can use them; the private `_sealed`
/// marker keeps the struct from being constructed anywhere outside this module.
#[doc(hidden)]
pub struct ScopedBackend<S> {
    /// Missing when the node runs without a Fiber actor (config-only RPC).
    actor: Option<FiberActorRef>,
    pub(crate) store: S,
    pub(crate) tenant: Option<TenantScope>,
    _sealed: (),
}

impl<S> ScopedBackend<S> {
    /// The Fiber actor this request runs against.
    ///
    /// Errors instead of panicking when the node has no actor: handlers that
    /// need one are then simply unavailable, which is what a config-only node
    /// should report.
    pub(crate) fn actor(&self) -> Result<&FiberActorRef, ErrorObjectOwned> {
        self.actor
            .as_ref()
            .ok_or_else(|| crate::rpc::utils::rpc_error("this node has no running Fiber actor"))
    }

    /// The actor, when the caller can run without one.
    pub(crate) fn actor_opt(&self) -> Option<&FiberActorRef> {
        self.actor.as_ref()
    }

    /// The tenant this request runs for, if any.
    pub(crate) fn tenant(&self) -> Option<&TenantScope> {
        self.tenant.as_ref()
    }
}

/// Resolves the caller's backend; the handler's only door to an actor.
pub(crate) struct BackendResolver<S> {
    public_actor: Option<FiberActorRef>,
    public_store: S,
    #[allow(dead_code)]
    lsp_actor: LspActorHandle,
}

impl<S: RpcBackendStore> BackendResolver<S> {
    pub(crate) fn new(
        public_actor: Option<FiberActorRef>,
        public_store: S,
        lsp_actor: LspActorHandle,
    ) -> Self {
        Self {
            public_actor,
            public_store,
            lsp_actor,
        }
    }

    /// The hosted LSP handle, for the one handler that queries the service
    /// directly instead of a tenant backend.
    #[cfg(all(not(target_arch = "wasm32"), feature = "lsp"))]
    pub(crate) fn lsp(&self) -> &LspActorHandle {
        &self.lsp_actor
    }

    /// Bind a hosted-delivery handle after construction.
    pub(crate) fn with_lsp_actor(mut self, lsp_actor: LspActorHandle) -> Self {
        self.lsp_actor = lsp_actor;
        self
    }

    /// The public backend, for callers that hold no request (wasm bindings).
    ///
    /// RPC handlers must not use this: they resolve through [`Self::resolve`] so
    /// that a tenant token can never reach the public node.
    pub fn public_scope(&self) -> ScopedBackend<S> {
        ScopedBackend {
            actor: self.public_actor.clone(),
            store: self.public_store.clone(),
            tenant: None,
            _sealed: (),
        }
    }

    /// Resolve the tenant selected by `extensions`, or the public node.
    pub(crate) async fn resolve(
        &self,
        extensions: &jsonrpsee::Extensions,
    ) -> Result<ScopedBackend<S>, ErrorObjectOwned> {
        Ok(
            match resolve_tenant_rpc_context(extensions, &self.lsp_actor).await? {
                Some(context) => Self::for_tenant(context, self.public_store.clone()),
                None => ScopedBackend {
                    actor: self.public_actor.clone(),
                    store: self.public_store.clone(),
                    tenant: None,
                    _sealed: (),
                },
            },
        )
    }

    /// Resolve a query target that may name a tenant explicitly.
    pub(crate) async fn resolve_query(
        &self,
        extensions: &jsonrpsee::Extensions,
        requested_tenant: Option<&str>,
    ) -> Result<ScopedBackend<S>, ErrorObjectOwned> {
        Ok(
            match crate::rpc::tenant::resolve_tenant_query_rpc_context(
                extensions,
                &self.lsp_actor,
                requested_tenant,
            )
            .await?
            {
                Some(context) => Self::for_tenant(context, self.public_store.clone()),
                None => ScopedBackend {
                    actor: self.public_actor.clone(),
                    store: self.public_store.clone(),
                    tenant: None,
                    _sealed: (),
                },
            },
        )
    }

    fn for_tenant(context: HostedTenantRpcContext, public_store: S) -> ScopedBackend<S> {
        ScopedBackend {
            actor: Some(context.fiber_actor),
            store: public_store.for_tenant(context.store),
            tenant: Some(TenantScope {
                tenant_id: context.tenant_id,
                public_node_id: context.public_node_id,
                config: context.config,
            }),
            _sealed: (),
        }
    }
}
