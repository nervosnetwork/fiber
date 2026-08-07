use std::{
    fs,
    path::{Path, PathBuf},
};

use clap_serde_derive::ClapSerde;

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
