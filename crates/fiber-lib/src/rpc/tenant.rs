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
#[cfg(feature = "lsp")]
use crate::rpc::utils::{rpc_error, RpcResultExt};
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

/// Reject tenant-scoped tokens that try to call a method outside the allowlist.
///
/// Operator tokens without an authority `tenant(...)` fact are unchanged.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn enforce_tenant_method_allowlist(method: &str, token: &Biscuit) -> anyhow::Result<()> {
    if extract_tenant_id(token)?.is_some() && !is_tenant_allowed_method(method) {
        bail!("tenant token is not allowed to call {method}");
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
    _extensions: &Extensions,
    _lsp_actor: &LspActorHandle,
) -> Result<Option<HostedTenantRpcContext>, ErrorObjectOwned> {
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
    _extensions: &Extensions,
    _lsp_actor: &LspActorHandle,
    _requested_tenant: Option<&str>,
) -> Result<Option<HostedTenantRpcContext>, ErrorObjectOwned> {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    #[cfg(feature = "lsp")]
    use std::sync::Arc;

    use biscuit_auth::{macros::biscuit, KeyPair};
    #[cfg(feature = "lsp")]
    use jsonrpsee::Extensions;

    use super::*;
    use crate::rpc::biscuit::BiscuitAuth;

    #[cfg(feature = "lsp")]
    fn authorization(root: &KeyPair, token: Option<String>) -> RpcAuthorization {
        RpcAuthorization {
            auth: Arc::new(BiscuitAuth::from_pubkey(root.public().to_string()).unwrap()),
            token,
        }
    }

    #[cfg(feature = "lsp")]
    fn tenant_extensions(root: &KeyPair, tenant: &str, token: &str) -> Extensions {
        let mut extensions = Extensions::new();
        extensions.insert(AuthenticatedTenant(TenantId::new(tenant).unwrap()));
        extensions.insert(authorization(root, Some(token.to_string())));
        extensions
    }

    /// Every source file under `src/rpc`, so the checks cannot drift from the code.
    fn rpc_sources() -> Vec<(PathBuf, String)> {
        fn visit(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
            for entry in std::fs::read_dir(dir).expect("read rpc directory") {
                let path = entry.expect("directory entry").path();
                if path.is_dir() {
                    visit(&path, out);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                    let source = std::fs::read_to_string(&path).expect("read rpc source");
                    out.push((path, source));
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/rpc");
        let mut sources = Vec::new();
        visit(&root, &mut sources);
        assert!(!sources.is_empty(), "no sources under {}", root.display());
        sources
    }

    /// Declaration text of every wire method, from its attribute to the `;` that
    /// ends the trait method. Enough to tell whether the handler is given the
    /// routing inputs a tenant-scoped call needs.
    ///
    /// Server and client (`#[rpc(client)]`) traits declare the same wire names; a
    /// name keeps the richest declaration seen, so the client trait cannot hide
    /// the server-side `with_extensions` / `RpcContext` parameters.
    /// `#[subscription(name = "subscribe", unsubscribe = "unsubscribe", ...)]`
    /// declares two wire names from one entry, so both map to it.
    fn declarations() -> BTreeMap<String, String> {
        let mut declarations: BTreeMap<String, String> = BTreeMap::new();
        for (_, source) in rpc_sources() {
            for marker in ["#[method(name = \"", "#[subscription(name = \""] {
                for (start, _) in source.match_indices(marker) {
                    let end = source[start..]
                        .find(';')
                        .expect("trait method declaration must end with `;`");
                    let declaration = &source[start..start + end];
                    let name = declaration[marker.len()..]
                        .split('"')
                        .next()
                        .expect("method name");
                    let mut insert = |name: &str, declaration: &str| {
                        let richer = declarations
                            .get(name)
                            .map(|existing| {
                                declares_tenant_routing(declaration)
                                    && !declares_tenant_routing(existing)
                            })
                            .unwrap_or(true);
                        if richer {
                            declarations.insert(name.to_string(), declaration.to_string());
                        }
                    };
                    insert(name, declaration);
                    if let Some(rest) = declaration.split("unsubscribe = \"").nth(1) {
                        let unsubscribe = rest.split('"').next().expect("unsubscribe name");
                        insert(unsubscribe, declaration);
                    }
                }
            }
        }
        assert!(!declarations.is_empty(), "no RPC declarations found");
        declarations
    }

    /// Whether the trait method is handed a way to see the tenant: the RPC
    /// `Extensions` (requested with `with_extensions`) or the middleware-injected
    /// `RpcContext`.
    ///
    /// `with_extensions` is the only declaration-level evidence available for the
    /// extensions path: the trait method itself never spells out `&Extensions`,
    /// only the generated impl does.
    fn declares_tenant_routing(declaration: &str) -> bool {
        declaration.contains("with_extensions") || declaration.contains("RpcContext")
    }

    fn reason<'a>(entries: &'a [(&str, &str)], method: &str) -> Option<&'a str> {
        entries
            .iter()
            .find(|(name, _)| *name == method)
            .map(|(_, reason)| *reason)
    }

    #[test]
    fn every_rpc_method_is_classified() {
        let declarations = declarations();
        let mut classified = std::collections::BTreeSet::new();
        for method in TENANT_SCOPED_METHODS.iter().chain(OPERATOR_ONLY_METHODS) {
            assert!(
                classified.insert(*method),
                "`{method}` is listed in both TENANT_SCOPED_METHODS and OPERATOR_ONLY_METHODS"
            );
        }

        let unclassified: Vec<_> = declarations
            .keys()
            .filter(|name| !classified.contains(name.as_str()))
            .collect();
        assert!(
            unclassified.is_empty(),
            "these RPC methods are classified in neither TENANT_SCOPED_METHODS nor \
             OPERATOR_ONLY_METHODS: {unclassified:?}\n\
             Unclassified methods are denied to tenant tokens, so put each one in one of \
             the two lists."
        );

        let stale: Vec<_> = classified
            .iter()
            .filter(|name| !declarations.contains_key(**name))
            .collect();
        assert!(
            stale.is_empty(),
            "these classified methods no longer exist on the RPC surface: {stale:?}"
        );

        for (name, methods) in [
            ("TENANT_SCOPED_METHODS", TENANT_SCOPED_METHODS),
            ("OPERATOR_ONLY_METHODS", OPERATOR_ONLY_METHODS),
        ] {
            let mut sorted = methods.to_vec();
            sorted.sort_unstable();
            assert_eq!(sorted, methods, "keep {name} sorted by method name");
        }
    }

    #[test]
    fn tenant_scoped_methods_declare_tenant_routing() {
        let declarations = declarations();
        for method in TENANT_SCOPED_METHODS {
            let declaration = declarations
                .get(*method)
                .unwrap_or_else(|| panic!("`{method}` has no declaration"));
            assert!(
                declares_tenant_routing(declaration),
                "`{method}` is reachable by tenant tokens but its trait method never receives \
                 `Extensions` or an injected `RpcContext`, so the handler cannot route to the \
                 tenant and would run against the public node."
            );
        }
    }

    #[test]
    fn operator_methods_that_can_route_tenants_are_declared() {
        let declarations = declarations();
        for (method, declaration) in &declarations {
            if !declares_tenant_routing(declaration) {
                continue;
            }
            if scope_of(method) == Some(RpcMethodScope::TenantScoped) {
                continue;
            }
            assert!(
                reason(TENANT_ROUTING_OPERATOR_METHODS, method).is_some(),
                "`{method}` asks for tenant routing inputs but is not reachable by tenants. \
                 Either list it in TENANT_SCOPED_METHODS or declare it in \
                 TENANT_ROUTING_OPERATOR_METHODS with the reason tenants must not call it."
            );
        }

        for (method, _) in TENANT_ROUTING_OPERATOR_METHODS {
            let declaration = declarations
                .get(*method)
                .unwrap_or_else(|| panic!("`{method}` is declared but has no handler"));
            assert_eq!(
                scope_of(method),
                Some(RpcMethodScope::OperatorOnly),
                "`{method}` is listed as operator-only but is classified as tenant-scoped"
            );
            assert!(
                declares_tenant_routing(declaration),
                "`{method}` is listed in TENANT_ROUTING_OPERATOR_METHODS but no longer asks for \
                 tenant routing inputs; remove the entry"
            );
        }
    }

    /// `RpcContext` is injected by the middleware as the first positional
    /// parameter, and only when the method's auth rule asks for it. Either
    /// mismatch breaks the call at runtime instead of at compile time: a handler
    /// taking `RpcContext` without the rule never receives its parameters, and a
    /// rule asking for it on a handler that does not take it injects an extra one.
    #[test]
    fn rpc_context_declarations_match_auth_rules() {
        let declarations = declarations();
        let auth = BiscuitAuth::without_pubkey();
        for (method, declaration) in &declarations {
            let takes_context = declaration.contains("RpcContext");
            match auth.get_rule(method) {
                Ok(rule) => assert_eq!(
                    takes_context,
                    rule.require_rpc_context,
                    "`{method}` {} an injected `RpcContext` but its auth rule {} require one",
                    if takes_context {
                        "takes"
                    } else {
                        "does not take"
                    },
                    if rule.require_rpc_context {
                        "does"
                    } else {
                        "does not"
                    }
                ),
                Err(_) if takes_context => panic!(
                    "`{method}` takes an injected `RpcContext` but has no auth rule, so the \
                     middleware can never provide it"
                ),
                Err(_) => {}
            }
        }
    }

    #[test]
    fn tenant_scoped_methods_have_auth_rules() {
        let auth = BiscuitAuth::without_pubkey();
        for method in TENANT_SCOPED_METHODS {
            let rule = auth
                .get_rule(method)
                .unwrap_or_else(|_| panic!("missing biscuit rule for {method}"));
            if method.contains("watchtower") {
                assert!(
                    rule.require_rpc_context,
                    "{method} must bind the watchtower namespace through RpcContext"
                );
            }
        }
    }

    #[test]
    fn unclassified_methods_are_denied_to_tenants() {
        let root = KeyPair::new();
        let tenant = biscuit!(r#"tenant("u1");"#).build(&root).unwrap();
        assert_eq!(scope_of("not_a_method"), None);
        assert!(!is_tenant_allowed_method("not_a_method"));
        assert!(enforce_tenant_method_allowlist("not_a_method", &tenant).is_err());
    }

    #[test]
    fn tenant_tokens_are_limited_to_the_declared_scopes() {
        let root = KeyPair::new();
        let tenant = biscuit!(r#"tenant("u1");"#).build(&root).unwrap();
        let operator = biscuit!(r#"write("channels");"#).build(&root).unwrap();

        for method in TENANT_SCOPED_METHODS {
            enforce_tenant_method_allowlist(method, &tenant).unwrap();
        }
        for method in OPERATOR_ONLY_METHODS {
            assert!(!is_tenant_allowed_method(method));
            let error = enforce_tenant_method_allowlist(method, &tenant)
                .expect_err("tenant token must be rejected")
                .to_string();
            assert!(error.contains(method), "{error}");
            enforce_tenant_method_allowlist(method, &operator).unwrap();
        }
        // A dev-only RPC with no tenant routing stays operator-only even though the
        // e2e suite reaches it with the operator token.
        assert!(!is_tenant_allowed_method("submit_commitment_transaction"));
        assert!(enforce_tenant_method_allowlist("submit_commitment_transaction", &tenant).is_err());
    }

    #[test]
    fn attenuation_cannot_turn_an_operator_token_into_a_tenant() {
        use biscuit_auth::macros::block;

        let root = KeyPair::new();
        let operator = biscuit!(r#"write("channels");"#).build(&root).unwrap();
        let attenuated = operator
            .append(block!(r#"tenant("u1");"#))
            .expect("append untrusted tenant fact");
        enforce_tenant_method_allowlist("open_channel", &attenuated).unwrap();
    }

    #[cfg(feature = "lsp")]
    #[tokio::test]
    async fn query_tenant_target_policy_is_fail_closed() {
        let root = KeyPair::new();
        let tenant_token = biscuit!(r#"tenant("u1"); read("invoices"); read("lsp");"#)
            .build(&root)
            .unwrap()
            .to_base64()
            .unwrap();
        let invoice_operator = biscuit!(r#"read("invoices"); read("payments");"#)
            .build(&root)
            .unwrap()
            .to_base64()
            .unwrap();
        let lsp_operator = biscuit!(r#"read("invoices"); read("payments"); read("lsp");"#)
            .build(&root)
            .unwrap()
            .to_base64()
            .unwrap();

        // A tenant defaults to itself and may repeat itself explicitly. Without
        // an LSP actor the next step is context resolution, not a public fallback.
        for requested in [None, Some("u1")] {
            let extensions = tenant_extensions(&root, "u1", &tenant_token);
            let error = resolve_tenant_query_rpc_context(&extensions, &None, requested)
                .await
                .err()
                .expect("query must fail");
            assert!(error.to_string().contains("not enabled"), "{error}");
        }

        // Cross-tenant targeting is denied even when the tenant token carries the
        // operator read("lsp") capability.
        let extensions = tenant_extensions(&root, "u1", &tenant_token);
        let error = resolve_tenant_query_rpc_context(&extensions, &None, Some("u2"))
            .await
            .err()
            .expect("query must fail");
        assert!(error.to_string().contains("another tenant"), "{error}");

        // An operator selecting a tenant must satisfy both the ordinary query
        // rule and the additional read("lsp") capability.
        let mut extensions = Extensions::new();
        extensions.insert(authorization(&root, Some(invoice_operator)));
        let error = resolve_tenant_query_rpc_context(&extensions, &None, Some("u1"))
            .await
            .err()
            .expect("query must fail");
        assert!(error.message().contains("read(\"lsp\")"), "{error}");

        let mut extensions = Extensions::new();
        extensions.insert(authorization(&root, Some(lsp_operator)));
        let error = resolve_tenant_query_rpc_context(&extensions, &None, Some("u1"))
            .await
            .err()
            .expect("query must fail");
        assert!(error.to_string().contains("not enabled"), "{error}");

        // Auth-disabled local RPC may select a tenant but still cannot silently
        // fall back when the LSP service is absent.
        let mut extensions = Extensions::new();
        extensions.insert(authorization(&root, None));
        let error = resolve_tenant_query_rpc_context(&extensions, &None, Some("u1"))
            .await
            .err()
            .expect("query must fail");
        assert!(error.to_string().contains("not enabled"), "{error}");

        // Missing authorization context fails closed for explicit operator targets.
        let error = resolve_tenant_query_rpc_context(&Extensions::new(), &None, Some("u1"))
            .await
            .err()
            .expect("query must fail");
        assert!(
            error.to_string().contains("authorization context"),
            "{error}"
        );

        // Omitting the target keeps the public namespace, and an invalid target
        // is rejected instead of being ignored.
        assert!(
            resolve_tenant_query_rpc_context(&Extensions::new(), &None, None)
                .await
                .unwrap()
                .is_none()
        );
        let error = resolve_tenant_query_rpc_context(&Extensions::new(), &None, Some("bad/id"))
            .await
            .err()
            .expect("query must fail");
        assert!(error.to_string().contains("tenant id"), "{error}");
    }
}
