use anyhow::bail;
use biscuit_auth::Biscuit;
use jsonrpsee::{types::ErrorObjectOwned, Extensions};
use ractor::{call, ActorRef};

use crate::lsp::{HostedTenantRpcContext, LspServiceMessage, TenantId};
use crate::rpc::biscuit::extract_tenant_id;
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
    "open_channel_with_external_funding",
    "remove_preimage",
    "remove_watch_channel",
    "shutdown_channel",
    "submit_channel_signature",
    "submit_commitment_transaction",
    "submit_signed_funding_tx",
    "submit_watchtower_signature",
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
    let actor = lsp_actor.ok_or_else(|| rpc_error("hosted LSP service is not enabled"))?;
    call!(
        actor,
        LspServiceMessage::GetTenantRpcContext,
        tenant.0.clone()
    )
    .rpc_err()?
    .rpc_err()
    .map(Some)
}

#[cfg(test)]
mod tests {
    use biscuit_auth::{macros::biscuit, KeyPair};

    use super::{
        enforce_tenant_method_allowlist, is_tenant_allowed_method, TENANT_ALLOWED_METHODS,
    };
    use crate::rpc::biscuit::BiscuitAuth;

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
            "new_invoice",
            "send_payment",
            "node_info",
            "lsp_register_tenant",
            "lsp_ensure_tenant",
            "lsp_evict_tenant",
            "lsp_list_tenants",
            "lsp_new_invoice",
            "lsp_send_payment",
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
}
