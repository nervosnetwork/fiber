use std::{path::Path, str::FromStr, sync::Arc};

use anyhow::{bail, Context, Result};
use biscuit_auth::{
    builder::{Fact, Term},
    Biscuit, KeyPair, PrivateKey, PublicKey,
};

use super::TenantId;
use fiber_types::NodeId;

const TENANT_CAPABILITIES: [(&str, &str); 8] = [
    ("read", "channels"),
    ("write", "channels"),
    ("read", "invoices"),
    ("write", "invoices"),
    ("read", "payments"),
    ("write", "payments"),
    ("read", "watchtower"),
    ("write", "watchtower"),
];

/// Signs tenant-scoped Biscuit access tokens with the RPC authentication root key.
#[derive(Clone)]
pub struct BiscuitTokenIssuer {
    root: Arc<KeyPair>,
}

impl BiscuitTokenIssuer {
    /// Load the issuer from a private-key file and check it matches `expected_public_key`.
    pub fn from_private_key_file(path: &Path, expected_public_key: &str) -> Result<Self> {
        let private_key = std::fs::read_to_string(path).with_context(|| {
            format!("failed to read Biscuit private key from {}", path.display())
        })?;
        Self::from_private_key(private_key.trim(), expected_public_key)
    }

    /// Construct an issuer from a prefixed private-key string.
    pub fn from_private_key(private_key: &str, expected_public_key: &str) -> Result<Self> {
        let private_key =
            PrivateKey::from_str(private_key).context("invalid Biscuit private key")?;
        let root = KeyPair::from(&private_key);
        let expected_public_key =
            PublicKey::from_str(expected_public_key).context("invalid Biscuit public key")?;
        if root.public() != expected_public_key {
            bail!("Biscuit private key does not match rpc.biscuit_public_key");
        }
        Ok(Self {
            root: Arc::new(root),
        })
    }

    /// Build a tenant-scoped access token. This is the only token-minting implementation.
    pub fn issue_tenant_token(&self, tenant_id: &TenantId, node_id: &NodeId) -> Result<String> {
        let mut builder = Biscuit::builder()
            .fact(Fact::new(
                "tenant".to_string(),
                &[Term::Str(tenant_id.as_str().to_string())],
            ))?
            .fact(Fact::new(
                "node".to_string(),
                &[Term::Str(node_id.to_string())],
            ))?;
        for (operation, resource) in TENANT_CAPABILITIES {
            builder = builder.fact(Fact::new(
                operation.to_string(),
                &[Term::Str(resource.to_string())],
            ))?;
        }
        // E2E nodes build with `debug-add-tlc`, which is forbidden in release.
        // Grant `write("dev")` so Bruno can kick `check_channel_shutdown`
        // instead of waiting for the 300s background scan.
        #[cfg(all(debug_assertions, feature = "debug-add-tlc"))]
        {
            builder = builder.fact(Fact::new(
                "write".to_string(),
                &[Term::Str("dev".to_string())],
            ))?;
        }
        builder
            .build(&self.root)?
            .to_base64()
            .context("failed to encode tenant Biscuit token")
    }
}
