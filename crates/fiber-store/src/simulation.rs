//! Minimal dependency-injection seam for SQLite VFS and batch fault hooks in simulation.

use std::any::Any;
use std::fmt::Debug;
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// Hook notified right before a SQLite write batch commits.
pub trait BatchCommitHook: Send + Sync + Debug {
    /// Inspect the batch operations and optionally return a scope guard kept alive
    /// for the duration of the SQLite transaction commit.
    fn enter_batch(
        &self,
        has_put_prefix: &dyn Fn(&[u8]) -> bool,
        has_delete_prefix: &dyn Fn(&[u8]) -> bool,
    ) -> Option<Box<dyn Any>>;
}

/// Pair of registered SQLite VFS name and its associated batch commit hook.
pub type VfsBinding = (&'static str, Arc<dyn BatchCommitHook>);

/// Resolves an optional SQLite VFS binding for a database directory.
pub type VfsResolver = dyn Fn(&Path) -> Result<Option<VfsBinding>, String> + Send + Sync;

static VFS_RESOLVER: OnceLock<Arc<VfsResolver>> = OnceLock::new();

/// Install the simulation SQLite VFS resolver once per process.
pub fn register_vfs_resolver(resolver: Arc<VfsResolver>) {
    let _ = VFS_RESOLVER.set(resolver);
}

pub(crate) fn vfs_for(path: &Path) -> Result<Option<VfsBinding>, String> {
    match VFS_RESOLVER.get() {
        Some(resolver) => resolver(path),
        None => Ok(None),
    }
}
