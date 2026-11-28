use std::path::{Path, PathBuf};

use clap_serde_derive::ClapSerde;

/// Default time an LSP may wait for an offline tenant before dispatching.
pub const DEFAULT_LSP_BUFFER_DURATION_MS: u64 = 24 * 60 * 60 * 1_000;
/// Protocol cap for invoice-requested offline buffering.
pub const MAX_LSP_BUFFER_DURATION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// Default upper bound for simultaneously active hosted tenant runtimes.
pub const DEFAULT_MAX_ACTIVE_TENANTS: usize = 64;
/// Default global bound for non-final hosted deliveries.
pub const DEFAULT_MAX_PENDING_DELIVERIES: usize = 1_024;
/// Default per-tenant bound for non-final hosted deliveries.
pub const DEFAULT_MAX_PENDING_DELIVERIES_PER_TENANT: usize = 64;
/// Default maximum watched channels owned by one tenant/node.
pub const DEFAULT_MAX_WATCH_CHANNELS_PER_TENANT: usize = 64;
/// Default global maximum watched channels across all nodes.
pub const DEFAULT_MAX_WATCH_CHANNELS: usize = 4_096;
/// Default maximum watchtower preimages owned by one tenant/node.
pub const DEFAULT_MAX_WATCH_PREIMAGES_PER_TENANT: usize = 256;
/// Default global maximum watchtower preimages across all nodes.
pub const DEFAULT_MAX_WATCH_PREIMAGES: usize = 16_384;
/// Default lifetime of a hosted tenant access token: thirty days.
pub const DEFAULT_LSP_TOKEN_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Per-node and global bounds for Watchtower channel and preimage records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchtowerQuotas {
    /// Maximum watched channels owned by a single node/tenant.
    pub max_channels_per_node: usize,
    /// Maximum watched channels stored globally across all nodes.
    pub max_channels_total: usize,
    /// Maximum watchtower preimages owned by a single node/tenant.
    pub max_preimages_per_node: usize,
    /// Maximum watchtower preimages stored globally across all nodes.
    pub max_preimages_total: usize,
}

impl Default for WatchtowerQuotas {
    fn default() -> Self {
        Self {
            max_channels_per_node: DEFAULT_MAX_WATCH_CHANNELS_PER_TENANT,
            max_channels_total: DEFAULT_MAX_WATCH_CHANNELS,
            max_preimages_per_node: DEFAULT_MAX_WATCH_PREIMAGES_PER_TENANT,
            max_preimages_total: DEFAULT_MAX_WATCH_PREIMAGES,
        }
    }
}

/// Configuration for the multi-tenant LSP service hosted by a Fiber node.
#[derive(ClapSerde, Debug, Clone)]
pub struct LspConfig {
    /// Lifetime of newly issued tenant access tokens, in seconds. Must be positive.
    #[default(DEFAULT_LSP_TOKEN_TTL_SECS)]
    #[arg(name = "LSP_TOKEN_TTL_SECS", long = "lsp-token-ttl-secs", env)]
    pub token_ttl_secs: u64,

    /// Base directory for tenant-local runtime files such as signing keys.
    /// LSP database state is stored in namespaces of the Fiber store.
    #[arg(
        name = "LSP_BASE_DIR",
        long = "lsp-base-dir",
        env,
        help = "base directory for LSP tenant runtime files [default: $BASE_DIR/lsp]"
    )]
    pub(crate) base_dir: Option<PathBuf>,

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

    /// Maximum number of watchtower channels owned by one tenant/node.
    #[default(DEFAULT_MAX_WATCH_CHANNELS_PER_TENANT)]
    #[arg(
        name = "LSP_MAX_WATCH_CHANNELS_PER_TENANT",
        long = "lsp-max-watch-channels-per-tenant",
        env,
        help = "maximum number of watchtower channels per tenant"
    )]
    pub max_watch_channels_per_tenant: usize,

    /// Maximum number of watchtower channels stored across all tenants.
    #[default(DEFAULT_MAX_WATCH_CHANNELS)]
    #[arg(
        name = "LSP_MAX_WATCH_CHANNELS",
        long = "lsp-max-watch-channels",
        env,
        help = "maximum number of watchtower channels globally"
    )]
    pub max_watch_channels: usize,

    /// Maximum number of watchtower preimages owned by one tenant/node.
    #[default(DEFAULT_MAX_WATCH_PREIMAGES_PER_TENANT)]
    #[arg(
        name = "LSP_MAX_WATCH_PREIMAGES_PER_TENANT",
        long = "lsp-max-watch-preimages-per-tenant",
        env,
        help = "maximum number of watchtower preimages per tenant"
    )]
    pub max_watch_preimages_per_tenant: usize,

    /// Maximum number of watchtower preimages stored across all tenants.
    #[default(DEFAULT_MAX_WATCH_PREIMAGES)]
    #[arg(
        name = "LSP_MAX_WATCH_PREIMAGES",
        long = "lsp-max-watch-preimages",
        env,
        help = "maximum number of watchtower preimages globally"
    )]
    pub max_watch_preimages: usize,
}

impl LspConfig {
    /// Return the configured Watchtower per-node and global quotas.
    pub fn watchtower_quotas(&self) -> WatchtowerQuotas {
        WatchtowerQuotas {
            max_channels_per_node: self.max_watch_channels_per_tenant,
            max_channels_total: self.max_watch_channels,
            max_preimages_per_node: self.max_watch_preimages_per_tenant,
            max_preimages_total: self.max_watch_preimages,
        }
    }
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
        if self.token_ttl_secs == 0
            || self
                .token_ttl_secs
                .checked_mul(1_000)
                .and_then(|ttl| crate::now_timestamp_as_millis_u64().checked_add(ttl))
                .is_none()
        {
            return Err(
                "LSP token_ttl_secs must be positive and fit a Unix millisecond timestamp"
                    .to_string(),
            );
        }
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
        if self.max_watch_channels_per_tenant == 0 || self.max_watch_channels == 0 {
            return Err("LSP watchtower channel quotas must be greater than zero".to_string());
        }
        if self.max_watch_channels_per_tenant > self.max_watch_channels {
            return Err(
                "LSP max_watch_channels_per_tenant cannot exceed max_watch_channels".to_string(),
            );
        }
        if self.max_watch_preimages_per_tenant == 0 || self.max_watch_preimages == 0 {
            return Err("LSP watchtower preimage quotas must be greater than zero".to_string());
        }
        if self.max_watch_preimages_per_tenant > self.max_watch_preimages {
            return Err(
                "LSP max_watch_preimages_per_tenant cannot exceed max_watch_preimages".to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
