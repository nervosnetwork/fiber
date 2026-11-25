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

    pub(crate) const fn physical_prefix() -> &'static [u8] {
        NODE_NAMESPACE_KEY_PREFIX
    }

    /// Extract a hosted tenant namespace from a physical store key, if the key
    /// belongs to one.
    pub(crate) fn parse_hosted_tenant_from_physical_key(key: &[u8]) -> Option<Self> {
        let rest = key.strip_prefix(NODE_NAMESPACE_KEY_PREFIX)?;
        let len_bytes: [u8; 4] = rest.get(..4)?.try_into().ok()?;
        let len = u32::from_be_bytes(len_bytes) as usize;
        let ns_bytes = rest.get(4..4usize.checked_add(len)?)?;
        let ns = std::str::from_utf8(ns_bytes).ok()?;
        let tenant_id = ns.strip_prefix("hosted-tenant/")?;
        Some(Self::hosted_tenant(tenant_id))
    }
}
