use std::{
    fs,
    path::{Path, PathBuf},
};

use clap_serde_derive::ClapSerde;

/// Default upper bound for simultaneously active hosted tenant runtimes.
pub const DEFAULT_MAX_ACTIVE_TENANTS: usize = 64;

/// Configuration for the multi-tenant LSP service hosted by a Fiber node.
#[derive(ClapSerde, Debug, Clone)]
pub struct LspConfig {
    /// Base directory for LSP-owned state.
    #[arg(
        name = "LSP_BASE_DIR",
        long = "lsp-base-dir",
        env,
        help = "base directory for LSP state [default: $BASE_DIR/lsp]"
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
}

impl LspConfig {
    /// Return the configured LSP base directory.
    pub fn base_dir(&self) -> &Path {
        self.base_dir.as_deref().expect("have set LSP base dir")
    }

    /// Return the database path reserved for LSP service metadata.
    pub fn store_path(&self) -> PathBuf {
        let path = self.base_dir().join("store");
        if !path.exists() {
            fs::create_dir_all(&path).expect("create LSP store directory");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
        }
        path
    }

    /// Return the root directory below which hosted tenant stores are created.
    pub fn tenant_store_root(&self) -> PathBuf {
        self.base_dir().join("tenants")
    }

    /// Validate limits that protect the process-wide hosted service.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_active_tenants == 0 {
            return Err("LSP max_active_tenants must be greater than zero".to_string());
        }
        Ok(())
    }

    /// Ensure the service metadata never shares a physical store with Public T.
    pub fn validate_store_separation(&self, public_store_path: &Path) -> Result<(), String> {
        if self.store_path() == public_store_path {
            return Err(
                "LSP service store must be separate from the public Fiber store".to_string(),
            );
        }
        Ok(())
    }
}
