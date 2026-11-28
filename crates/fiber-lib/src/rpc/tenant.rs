#![cfg_attr(target_arch = "wasm32", allow(dead_code))] // wasm has no tenant scoping
//! Tenancy declarations and tenant-backend resolution for the JSON-RPC surface.
//!
//! Tenancy used to be two separate things that had to stay in sync by hand: an
//! allowlist of method names in this module and a doc comment asking every
//! handler to remember its own `resolve_tenant_rpc_context` call. Both halves
//! fail silently, and the failure mode is a tenant token reading or writing the
//! public node's state.
//!
//! Tenancy is therefore declared once, per wire method, as either
//! [`TENANT_SCOPED_METHODS`] or [`OPERATOR_ONLY_METHODS`], and the declarations
//! are cross-checked against the RPC surface by the tests at the bottom of this
//! file:
//!
//! * `every_rpc_method_is_classified` — a new, removed or renamed `#[method]` /
//!   `#[subscription]` fails until it appears in exactly one of the two lists; a
//!   method in neither stays deny-all for tenant tokens.
//! * `tenant_scoped_methods_declare_tenant_routing` — a method tenant tokens may
//!   call must ask for `with_extensions` or an injected `RpcContext` in its trait
//!   declaration. Without those the handler physically cannot see the tenant and
//!   would serve a tenant token from the public node.
//! * `operator_methods_that_can_route_tenants_are_declared` — the same inputs on
//!   an operator-only method must be acknowledged in
//!   [`TENANT_ROUTING_OPERATOR_METHODS`] with a reason.
//! * `rpc_context_declarations_match_auth_rules` — a handler taking the injected
//!   `RpcContext` and the auth rule that asks the middleware to inject it must
//!   agree in both directions.
//!
//! These are declaration-level checks: they prove a handler *can* route to a
//! tenant, not that it does. Keeping the routing visible is still a review
//! question; the tests exist so that forgetting to give a handler the inputs
//! fails CI instead of leaking public state silently.

#[cfg(not(target_arch = "wasm32"))]
use anyhow::{anyhow, bail};
#[cfg(not(target_arch = "wasm32"))]
use biscuit_auth::Biscuit;
use jsonrpsee::{types::ErrorObjectOwned, Extensions};

use crate::fiber::network::FiberActorRef;
#[cfg(not(target_arch = "wasm32"))]
use crate::fiber_types::NodeId;
use crate::fiber_types::{Pubkey, TenantId};
#[cfg(not(target_arch = "wasm32"))]
use crate::rpc::biscuit::{extract_optional_node_id, extract_tenant_id};
#[cfg(feature = "lsp")]
use crate::rpc::middleware::RpcAuthorization;
use crate::rpc::utils::rpc_error;
#[cfg(feature = "lsp")]
use crate::rpc::utils::RpcResultExt;
use crate::store::Store;
use crate::FiberConfig;

#[cfg(feature = "lsp")]
use crate::lsp::LspServiceMessage;
#[cfg(feature = "lsp")]
use ractor::{call, ActorRef};

/// Handle to the hosted LSP service.
///
/// With the `lsp` feature off there is no service to hold, so the alias becomes
/// `Option<UnavailableLspActor>`: the value can only be `None`, which keeps every
/// caller signature identical while making a real handle impossible to construct.
#[cfg(feature = "lsp")]
pub(crate) type LspActorHandle = Option<ActorRef<LspServiceMessage>>;
#[cfg(not(feature = "lsp"))]
pub(crate) type LspActorHandle = Option<UnavailableLspActor>;

/// Uninhabited stand-in for the LSP service handle.
///
/// Not `Copy`, so call sites that clone the handle keep compiling, and not
/// constructible, so no build without the `lsp` feature can install one.
#[cfg(not(feature = "lsp"))]
#[doc(hidden)]
#[derive(Clone, Debug)]
pub enum UnavailableLspActor {}

/// Tenant-scoped handles used by the authenticated LSP RPC facade.
///
/// Lives here rather than in the LSP module so the RPC layer keeps its signature
/// when the `lsp` feature is off; nothing can populate it in that build.
#[doc(hidden)]
#[derive(Clone)]
pub struct HostedTenantRpcContext {
    // Only the LSP-feature code paths read the identity back out.
    #[cfg_attr(not(feature = "lsp"), allow(dead_code))]
    pub(crate) tenant_id: TenantId,
    pub(crate) config: FiberConfig,
    pub(crate) fiber_actor: FiberActorRef,
    pub(crate) public_node_id: Pubkey,
    pub(crate) store: Store,
}

#[cfg(not(target_arch = "wasm32"))]
/// Resolve the watchtower namespace for an authenticated token.
///
/// Tenant tokens may only touch the `node(...)` bound at issue time, which is
/// the hosted tenant watchtower id. They cannot address `NodeId::local()`.
/// Operator tokens without a node fact use the host namespace.
pub fn scoped_rpc_node_id(token: &Biscuit) -> anyhow::Result<NodeId> {
    match (extract_tenant_id(token)?, extract_optional_node_id(token)?) {
        (Some(_), Some(node_id)) if node_id != NodeId::local() => Ok(node_id),
        (Some(_), Some(_)) => Err(anyhow!(
            "tenant token cannot access the host watchtower namespace"
        )),
        (Some(_), None) => Err(anyhow!("tenant token is missing a node fact")),
        (None, Some(node_id)) => Ok(node_id),
        (None, None) => Ok(NodeId::local()),
    }
}

/// Tenant identity extracted from the authority block of an authenticated
/// Biscuit token. Request parameters cannot construct this extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthenticatedTenant(pub(crate) TenantId);

/// How a JSON-RPC method is scoped to hosted tenants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RpcMethodScope {
    /// Operator/local RPC only. Tenant tokens are rejected before the handler runs.
    OperatorOnly,
    /// Reachable by tenant tokens. The handler must resolve its tenant route.
    TenantScoped,
}

/// Wire names a hosted tenant token may call.
///
/// This is the security-critical list. Everything not listed here is rejected
/// for tenant tokens, including methods that are not classified at all.
pub(crate) const TENANT_SCOPED_METHODS: &[&str] = &[
    "cancel_invoice",
    "check_channel_shutdown",
    "create_preimage",
    "create_watch_channel",
    "get_channel_signing_status",
    "get_invoice",
    "get_payment",
    "get_watchtower_signing_status",
    "list_channels",
    "list_payments",
    "new_invoice",
    "open_channel_with_external_funding",
    "remove_preimage",
    "remove_watch_channel",
    "send_payment",
    "settle_invoice",
    "shutdown_channel",
    "submit_channel_signature",
    "submit_signed_funding_tx",
    "submit_watchtower_signature",
    "subscribe",
    "unsubscribe",
    "update_channel",
];

/// Wire names reserved for operator/local RPC.
///
/// Listing them explicitly keeps the decision reviewable: a new method has to be
/// put in this list or in [`TENANT_SCOPED_METHODS`], never merely left out.
pub(crate) const OPERATOR_ONLY_METHODS: &[&str] = &[
    "abandon_channel",
    "accept_channel",
    "add_tlc",
    "backup",
    "build_router",
    "commitment_signed",
    "connect_peer",
    "disconnect_peer",
    "get_cch_order",
    "graph_channels",
    "graph_nodes",
    "list_peers",
    "lsp_ensure_tenant",
    "lsp_evict_tenant",
    "lsp_get_payment_delivery",
    "lsp_get_status",
    "lsp_list_tenants",
    "lsp_register_tenant",
    "lsp_revoke_tenant_tokens",
    "node_info",
    "open_channel",
    "parse_invoice",
    "pprof",
    "receive_btc",
    "remove_tlc",
    "send_btc",
    "send_payment_with_router",
    "sign_external_funding_tx",
    "submit_commitment_transaction",
    "update_local_settlement",
    "update_pending_remote_settlement",
    "update_revocation",
];

/// Operator-only methods that ask for tenant routing inputs anyway.
///
/// These handlers receive `Extensions` or an injected
/// [`fiber_json_types::RpcContext`] and route (or validate) tenant state, but a
/// tenant token cannot reach them. Declaring them keeps the difference between
/// "not exposed" and "deliberately operator-only" reviewable:
/// `operator_methods_that_can_route_tenants_are_declared` fails when a method
/// asks for routing inputs without being classified either way.
#[cfg(test)]
pub(crate) const TENANT_ROUTING_OPERATOR_METHODS: &[(&str, &str)] = &[
    (
        "abandon_channel",
        "routes tenant requests, but abandoning a hosted channel while the LSP is still \
         negotiating the funding would desync its channel-open record",
    ),
    (
        "accept_channel",
        "routes tenant requests, but it lacks the private-channel guard `open_channel` has; \
         not exposed until that guard exists",
    ),
    (
        "build_router",
        "routes tenant requests, but tenant payments route implicitly through `send_payment`, \
         so building routers stays operator-only by design",
    ),
    (
        "open_channel",
        "routes tenant requests, but a hosted tenant has no wallet to fund from; hosted \
         channels open through `open_channel_with_external_funding`",
    ),
    (
        "send_payment_with_router",
        "routes tenant requests, but tenant payments use `send_payment`'s auto-routing, so \
         explicit routers stay operator-only by design",
    ),
    (
        "update_local_settlement",
        "operates on the tenant watchtower namespace selected by RpcContext; tenants submit \
         signatures instead of updating settlement state",
    ),
    (
        "update_pending_remote_settlement",
        "operates on the tenant watchtower namespace selected by RpcContext; tenants submit \
         signatures instead of updating settlement state",
    ),
    (
        "update_revocation",
        "operates on the tenant watchtower namespace selected by RpcContext; tenants submit \
         signatures instead of updating revocation state",
    ),
];

/// Scope of `method`, or `None` when the method is not classified.
///
/// Callers must treat `None` as [`RpcMethodScope::OperatorOnly`]; the
/// classification test keeps this unreachable in practice.
pub(crate) fn scope_of(method: &str) -> Option<RpcMethodScope> {
    if TENANT_SCOPED_METHODS.contains(&method) {
        Some(RpcMethodScope::TenantScoped)
    } else if OPERATOR_ONLY_METHODS.contains(&method) {
        Some(RpcMethodScope::OperatorOnly)
    } else {
        None
    }
}

/// Whether a tenant token may call `method`.
///
/// Unclassified methods are denied: the default for a new method is "operator
/// only" until it is listed in [`TENANT_SCOPED_METHODS`].
pub(crate) fn is_tenant_allowed_method(method: &str) -> bool {
    scope_of(method) == Some(RpcMethodScope::TenantScoped)
}

/// Reject tenant tokens when hosting is unavailable or the method is outside the allowlist.
///
/// Operator tokens without an authority `tenant(...)` fact are unchanged.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn enforce_tenant_method_allowlist(method: &str, token: &Biscuit) -> anyhow::Result<()> {
    if extract_tenant_id(token)?.is_some() {
        if !cfg!(feature = "lsp") {
            bail!("tenant token cannot call {method} without LSP support");
        }
        if !is_tenant_allowed_method(method) {
            bail!("tenant token is not allowed to call {method}");
        }
    }
    Ok(())
}

/// Resolve the hosted tenant backend selected by an authenticated request.
/// Requests without a tenant identity continue to use the public node backend.
#[cfg(feature = "lsp")]
pub(crate) async fn resolve_tenant_rpc_context(
    extensions: &Extensions,
    lsp_actor: &LspActorHandle,
) -> Result<Option<HostedTenantRpcContext>, ErrorObjectOwned> {
    let Some(tenant) = extensions.get::<AuthenticatedTenant>() else {
        return Ok(None);
    };
    resolve_tenant_rpc_context_for(lsp_actor.as_ref(), tenant.0.clone())
        .await
        .map(Some)
}

/// Without the `lsp` feature no tenant backend can exist.
#[cfg(not(feature = "lsp"))]
pub(crate) async fn resolve_tenant_rpc_context(
    extensions: &Extensions,
    _lsp_actor: &LspActorHandle,
) -> Result<Option<HostedTenantRpcContext>, ErrorObjectOwned> {
    if extensions.get::<AuthenticatedTenant>().is_some() {
        return Err(rpc_error("hosted LSP service is not enabled"));
    }
    Ok(None)
}

/// Resolve a query target for the ordinary `get_invoice`/`get_payment` methods.
///
/// Tenant tokens default to their own namespace and may only name themselves.
/// Operator requests without a target use the public namespace; selecting any
/// tenant explicitly rechecks the `read("lsp")` capability. Auth-disabled local
/// RPC has no token and may select a tenant. A missing or unavailable target is
/// always an error rather than a fallback to public state.
#[cfg(feature = "lsp")]
pub(crate) async fn resolve_tenant_query_rpc_context(
    extensions: &Extensions,
    lsp_actor: &LspActorHandle,
    requested_tenant: Option<&str>,
) -> Result<Option<HostedTenantRpcContext>, ErrorObjectOwned> {
    let requested = requested_tenant
        .map(TenantId::new)
        .transpose()
        .map_err(rpc_error)?;

    match (extensions.get::<AuthenticatedTenant>(), requested) {
        (Some(authenticated), None) => {
            resolve_tenant_rpc_context_for(lsp_actor.as_ref(), authenticated.0.clone())
                .await
                .map(Some)
        }
        (Some(authenticated), Some(requested)) => {
            if authenticated.0 != requested {
                return Err(rpc_error(
                    "tenant token cannot query another tenant's state",
                ));
            }
            resolve_tenant_rpc_context_for(lsp_actor.as_ref(), requested)
                .await
                .map(Some)
        }
        (None, None) => Ok(None),
        (None, Some(requested)) => {
            authorize_explicit_tenant_query(extensions)?;
            resolve_tenant_rpc_context_for(lsp_actor.as_ref(), requested)
                .await
                .map(Some)
        }
    }
}

/// Without the `lsp` feature no tenant backend can exist.
#[cfg(not(feature = "lsp"))]
pub(crate) async fn resolve_tenant_query_rpc_context(
    extensions: &Extensions,
    _lsp_actor: &LspActorHandle,
    requested_tenant: Option<&str>,
) -> Result<Option<HostedTenantRpcContext>, ErrorObjectOwned> {
    if extensions.get::<AuthenticatedTenant>().is_some() || requested_tenant.is_some() {
        return Err(rpc_error("hosted LSP service is not enabled"));
    }
    Ok(None)
}

#[cfg(feature = "lsp")]
fn authorize_explicit_tenant_query(extensions: &Extensions) -> Result<(), ErrorObjectOwned> {
    let authorization = extensions
        .get::<RpcAuthorization>()
        .ok_or_else(|| rpc_error("RPC authorization context is missing"))?;
    // `authorize` returns None only for auth-disabled local RPC. An authenticated
    // operator must satisfy the existing read("lsp") policy for the LSP status
    // method; an authenticated tenant token cannot reach this branch.
    authorization
        .authorize("lsp_get_status")
        .map(|_| ())
        .map_err(|error| {
            rpc_error(format!(
                "explicit tenant_id requires read(\"lsp\") permission: {error}"
            ))
        })
}

#[cfg(feature = "lsp")]
async fn resolve_tenant_rpc_context_for(
    lsp_actor: Option<&ActorRef<LspServiceMessage>>,
    tenant: TenantId,
) -> Result<HostedTenantRpcContext, ErrorObjectOwned> {
    let actor = lsp_actor.ok_or_else(|| rpc_error("hosted LSP service is not enabled"))?;
    call!(actor, LspServiceMessage::GetTenantRpcContext, tenant)
        .rpc_err()?
        .rpc_err()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[cfg(test)]
mod tests;
