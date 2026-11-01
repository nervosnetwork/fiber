use anyhow::bail;
use biscuit_auth::Biscuit;
use jsonrpsee::{types::ErrorObjectOwned, Extensions};
use ractor::{call, ActorRef};

use crate::lsp::{HostedTenantRpcContext, LspServiceMessage, TenantId};
use crate::rpc::biscuit::extract_tenant_id;
use crate::rpc::middleware::RpcAuthorization;
use crate::rpc::utils::{rpc_error, RpcResultExt};

/// Tenant identity extracted from the authority block of an authenticated
/// Biscuit token. Request parameters cannot construct this extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthenticatedTenant(pub(crate) TenantId);

/// RPC methods a hosted tenant token may call.
///
/// Evaluated in RPC middleware after Biscuit capability checks. New methods are
/// denied for tenant tokens until added here. Keep this list sorted so
/// [`is_tenant_allowed_method`] can binary-search. Each entry must isolate
/// tenant state in its handler (`resolve_tenant_rpc_context` or
/// `scoped_rpc_node_id`).
pub(crate) const TENANT_ALLOWED_METHODS: &[&str] = &[
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
    "submit_commitment_transaction",
    "submit_signed_funding_tx",
    "submit_watchtower_signature",
    "subscribe",
    "unsubscribe",
    "update_channel",
];

pub(crate) fn is_tenant_allowed_method(method: &str) -> bool {
    TENANT_ALLOWED_METHODS.binary_search(&method).is_ok()
}

/// Reject tenant-scoped tokens that try to call a method outside the allowlist.
///
/// Operator tokens without an authority `tenant(...)` fact are unchanged.
pub(crate) fn enforce_tenant_method_allowlist(method: &str, token: &Biscuit) -> anyhow::Result<()> {
    if extract_tenant_id(token)?.is_some() && !is_tenant_allowed_method(method) {
        bail!("tenant token is not allowed to call {method}");
    }
    Ok(())
}

/// Resolve the hosted tenant backend selected by an authenticated request.
/// Requests without a tenant identity continue to use the public node backend.
pub(crate) async fn resolve_tenant_rpc_context(
    extensions: &Extensions,
    lsp_actor: Option<&ActorRef<LspServiceMessage>>,
) -> Result<Option<HostedTenantRpcContext>, ErrorObjectOwned> {
    let Some(tenant) = extensions.get::<AuthenticatedTenant>() else {
        return Ok(None);
    };
    resolve_tenant_rpc_context_for(lsp_actor, tenant.0.clone())
        .await
        .map(Some)
}

/// Resolve a query target for the ordinary `get_invoice`/`get_payment` methods.
///
/// Tenant tokens default to their own namespace and may only name themselves.
/// Operator requests without a target use the public namespace; selecting any
/// tenant explicitly rechecks the `read("lsp")` capability. Auth-disabled local
/// RPC has no token and may select a tenant. A missing or unavailable target is
/// always an error rather than a fallback to public state.
pub(crate) async fn resolve_tenant_query_rpc_context(
    extensions: &Extensions,
    lsp_actor: Option<&ActorRef<LspServiceMessage>>,
    requested_tenant: Option<&str>,
) -> Result<Option<HostedTenantRpcContext>, ErrorObjectOwned> {
    let requested = requested_tenant
        .map(TenantId::new)
        .transpose()
        .map_err(rpc_error)?;

    match (extensions.get::<AuthenticatedTenant>(), requested) {
        (Some(authenticated), None) => {
            resolve_tenant_rpc_context_for(lsp_actor, authenticated.0.clone())
                .await
                .map(Some)
        }
        (Some(authenticated), Some(requested)) => {
            if authenticated.0 != requested {
                return Err(rpc_error(
                    "tenant token cannot query another tenant's state",
                ));
            }
            resolve_tenant_rpc_context_for(lsp_actor, requested)
                .await
                .map(Some)
        }
        (None, None) => Ok(None),
        (None, Some(requested)) => {
            authorize_explicit_tenant_query(extensions)?;
            resolve_tenant_rpc_context_for(lsp_actor, requested)
                .await
                .map(Some)
        }
    }
}

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
    use std::sync::Arc;

    use biscuit_auth::{macros::biscuit, KeyPair};
    use jsonrpsee::Extensions;

    use super::{
        enforce_tenant_method_allowlist, is_tenant_allowed_method,
        resolve_tenant_query_rpc_context, AuthenticatedTenant, TENANT_ALLOWED_METHODS,
    };
    use crate::lsp::TenantId;
    use crate::rpc::biscuit::BiscuitAuth;
    use crate::rpc::middleware::RpcAuthorization;

    fn authorization(root: &KeyPair, token: Option<String>) -> RpcAuthorization {
        RpcAuthorization {
            auth: Arc::new(BiscuitAuth::from_pubkey(root.public().to_string()).unwrap()),
            token,
        }
    }

    fn tenant_extensions(root: &KeyPair, tenant: &str, token: &str) -> Extensions {
        let mut extensions = Extensions::new();
        extensions.insert(AuthenticatedTenant(TenantId::new(tenant).unwrap()));
        extensions.insert(authorization(root, Some(token.to_string())));
        extensions
    }

    #[test]
    fn tenant_allowed_methods_are_sorted_and_have_auth_rules() {
        let mut sorted = TENANT_ALLOWED_METHODS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, TENANT_ALLOWED_METHODS);

        let auth = BiscuitAuth::without_pubkey();
        for method in TENANT_ALLOWED_METHODS {
            let rule = auth
                .get_rule(method)
                .unwrap_or_else(|_| panic!("missing biscuit rule for {method}"));
            if method.contains("watchtower") {
                assert!(
                    rule.require_rpc_context,
                    "{method} must bind watchtower namespace via RpcContext"
                );
            }
        }
    }

    #[test]
    fn tenant_tokens_are_limited_to_the_allowlist() {
        let root = KeyPair::new();
        let tenant = biscuit!(r#"tenant("u1"); write("channels");"#)
            .build(&root)
            .unwrap();
        let operator = biscuit!(r#"write("channels");"#).build(&root).unwrap();

        for method in TENANT_ALLOWED_METHODS {
            assert!(is_tenant_allowed_method(method));
            enforce_tenant_method_allowlist(method, &tenant).unwrap();
            enforce_tenant_method_allowlist(method, &operator).unwrap();
        }

        for method in [
            "open_channel",
            "open_tenant_channel",
            "node_info",
            "lsp_register_tenant",
            "lsp_ensure_tenant",
            "lsp_evict_tenant",
            "lsp_list_tenants",
        ] {
            assert!(!is_tenant_allowed_method(method));
            assert!(enforce_tenant_method_allowlist(method, &tenant)
                .unwrap_err()
                .to_string()
                .contains(method));
            enforce_tenant_method_allowlist(method, &operator).unwrap();
        }
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
            let error = resolve_tenant_query_rpc_context(&extensions, None, requested)
                .await
                .err()
                .expect("query must fail");
            assert!(error.to_string().contains("not enabled"), "{error}");
        }

        // Cross-tenant targeting is denied even when the tenant token carries the
        // operator read("lsp") capability.
        let extensions = tenant_extensions(&root, "u1", &tenant_token);
        let error = resolve_tenant_query_rpc_context(&extensions, None, Some("u2"))
            .await
            .err()
            .expect("query must fail");
        assert!(error.to_string().contains("another tenant"), "{error}");

        // An operator selecting a tenant must satisfy both the ordinary query
        // rule and the additional read("lsp") capability.
        let mut extensions = Extensions::new();
        extensions.insert(authorization(&root, Some(invoice_operator)));
        let error = resolve_tenant_query_rpc_context(&extensions, None, Some("u1"))
            .await
            .err()
            .expect("query must fail");
        assert!(error.message().contains("read(\"lsp\")"), "{error}");

        let mut extensions = Extensions::new();
        extensions.insert(authorization(&root, Some(lsp_operator)));
        let error = resolve_tenant_query_rpc_context(&extensions, None, Some("u1"))
            .await
            .err()
            .expect("query must fail");
        assert!(error.to_string().contains("not enabled"), "{error}");

        // Auth-disabled local RPC may select a tenant but still cannot silently
        // fall back when the LSP service is absent.
        let mut extensions = Extensions::new();
        extensions.insert(authorization(&root, None));
        let error = resolve_tenant_query_rpc_context(&extensions, None, Some("u1"))
            .await
            .err()
            .expect("query must fail");
        assert!(error.to_string().contains("not enabled"), "{error}");

        // Missing authorization context fails closed for explicit operator targets.
        let error = resolve_tenant_query_rpc_context(&Extensions::new(), None, Some("u1"))
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
            resolve_tenant_query_rpc_context(&Extensions::new(), None, None)
                .await
                .unwrap()
                .is_none()
        );
        let error = resolve_tenant_query_rpc_context(&Extensions::new(), None, Some("bad/id"))
            .await
            .err()
            .expect("query must fail");
        assert!(error.to_string().contains("tenant id"), "{error}");
    }
}
