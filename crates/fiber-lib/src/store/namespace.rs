use std::sync::Arc;

const NODE_NAMESPACE_KEY_PREFIX: &[u8] = b"\xfffnn/node-namespace/v1/";

/// A logical state domain stored inside a shared physical Fiber store.
///
/// The hosted LSP service uses this boundary so its metadata and each tenant's
/// node state cannot alias Public T or another tenant even though all of them
/// share one database handle.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NodeNamespace(Arc<str>);

impl NodeNamespace {
    /// Create the namespace assigned to hosted LSP service metadata.
    pub fn lsp_metadata() -> Self {
        Self(Arc::from("lsp/metadata"))
    }

    /// Create the namespace assigned to one hosted tenant.
    pub fn hosted_tenant(tenant_id: &str) -> Self {
        Self(Arc::from(format!("hosted-tenant/{tenant_id}")))
    }

    pub(crate) fn key_prefix(&self) -> Vec<u8> {
        let namespace = self.0.as_bytes();
        let mut prefix = Vec::with_capacity(
            NODE_NAMESPACE_KEY_PREFIX.len() + std::mem::size_of::<u32>() + namespace.len(),
        );
        prefix.extend_from_slice(NODE_NAMESPACE_KEY_PREFIX);
        prefix.extend_from_slice(&(namespace.len() as u32).to_be_bytes());
        prefix.extend_from_slice(namespace);
        prefix
    }
}
