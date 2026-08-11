use jsonrpsee::{types::ErrorObjectOwned, Extensions};
use ractor::{call, ActorRef};

use crate::lsp::{HostedTenantRpcContext, LspServiceMessage, TenantId};
use crate::rpc::utils::{rpc_error, RpcResultExt};

/// Tenant identity extracted from the authority block of an authenticated
/// Biscuit token. Request parameters cannot construct this extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthenticatedTenant(pub(crate) TenantId);

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
