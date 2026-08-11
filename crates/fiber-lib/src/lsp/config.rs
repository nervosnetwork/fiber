use std::path::{Path, PathBuf};

use clap_serde_derive::ClapSerde;

use super::MAX_LSP_BUFFER_DURATION_MS;

/// Default upper bound for simultaneously active hosted tenant runtimes.
pub const DEFAULT_MAX_ACTIVE_TENANTS: usize = 64;
/// Default global bound for non-final hosted deliveries.
pub const DEFAULT_MAX_PENDING_DELIVERIES: usize = 1_024;
/// Default per-tenant bound for non-final hosted deliveries.
pub const DEFAULT_MAX_PENDING_DELIVERIES_PER_TENANT: usize = 64;

/// Configuration for the multi-tenant LSP service hosted by a Fiber node.
#[derive(ClapSerde, Debug, Clone)]
pub struct LspConfig {
    /// Base directory for tenant-local runtime files such as signing keys.
    /// LSP database state is stored in namespaces of the Fiber store.
    #[arg(
        name = "LSP_BASE_DIR",
        long = "lsp-base-dir",
        env,
        help = "base directory for LSP tenant runtime files [default: $BASE_DIR/lsp]"
    )]
    pub(crate) base_dir: Option<PathBuf>,

    /// Tenant identifiers provisioned when the service starts. Provisioning is
    /// persistent but does not eagerly start their Fiber runtimes.
    #[arg(skip)]
    #[serde(default)]
    pub tenants: Vec<String>,

    /// Maximum number of tenant Fiber runtimes kept active at once.
    #[default(DEFAULT_MAX_ACTIVE_TENANTS)]
    #[arg(
        name = "LSP_MAX_ACTIVE_TENANTS",
        long = "lsp-max-active-tenants",
        env,
        help = "maximum number of active hosted tenant runtimes"
    )]
    pub max_active_tenants: usize,

    /// Operator policy cap for invoice-requested offline buffering.
    #[default(MAX_LSP_BUFFER_DURATION_MS)]
    #[arg(
        name = "LSP_MAX_BUFFER_DURATION_MS",
        long = "lsp-max-buffer-duration-ms",
        env,
        help = "maximum hosted payment buffer duration in milliseconds"
    )]
    pub max_buffer_duration_ms: u64,

    /// Maximum number of non-final hosted deliveries across all tenants.
    #[default(DEFAULT_MAX_PENDING_DELIVERIES)]
    #[arg(
        name = "LSP_MAX_PENDING_DELIVERIES",
        long = "lsp-max-pending-deliveries",
        env,
        help = "maximum number of pending hosted deliveries"
    )]
    pub max_pending_deliveries: usize,

    /// Maximum number of non-final hosted deliveries owned by one tenant.
    #[default(DEFAULT_MAX_PENDING_DELIVERIES_PER_TENANT)]
    #[arg(
        name = "LSP_MAX_PENDING_DELIVERIES_PER_TENANT",
        long = "lsp-max-pending-deliveries-per-tenant",
        env,
        help = "maximum number of pending hosted deliveries per tenant"
    )]
    pub max_pending_deliveries_per_tenant: usize,
}

impl LspConfig {
    /// Return the configured LSP base directory.
    pub fn base_dir(&self) -> &Path {
        self.base_dir.as_deref().expect("have set LSP base dir")
    }

    /// Return the root directory below which tenant-local runtime files are created.
    pub fn tenant_store_root(&self) -> PathBuf {
        self.base_dir().join("tenants")
    }

    /// Validate limits that protect the process-wide hosted service.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_active_tenants == 0 {
            return Err("LSP max_active_tenants must be greater than zero".to_string());
        }
        if self.max_buffer_duration_ms == 0
            || self.max_buffer_duration_ms > MAX_LSP_BUFFER_DURATION_MS
        {
            return Err(format!(
                "LSP max_buffer_duration_ms must be in range [1, {}]",
                MAX_LSP_BUFFER_DURATION_MS
            ));
        }
        if self.max_pending_deliveries == 0 {
            return Err("LSP max_pending_deliveries must be greater than zero".to_string());
        }
        if self.max_pending_deliveries_per_tenant == 0 {
            return Err(
                "LSP max_pending_deliveries_per_tenant must be greater than zero".to_string(),
            );
        }
        Ok(())
    }
}
