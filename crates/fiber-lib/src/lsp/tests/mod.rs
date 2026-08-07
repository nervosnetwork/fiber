use std::path::PathBuf;

use fiber_store::backend::StorageBackend;
use tempfile::tempdir;

use crate::lsp::LspConfig;
use crate::store::open_store;

#[test]
fn lsp_service_uses_an_independent_store() {
    let root = tempdir().expect("temporary directory");
    let public_store_path = root.path().join("fiber/store");
    let config = LspConfig {
        base_dir: Some(root.path().join("lsp")),
    };

    config
        .validate_store_separation(&public_store_path)
        .expect("separate store paths");

    std::fs::create_dir_all(&public_store_path).expect("create public store path");
    let public_store = open_store(&public_store_path).expect("open public store");
    let lsp_store = open_store(config.store_path()).expect("open LSP store");
    public_store.put(b"same-key", b"public-value");
    lsp_store.put(b"same-key", b"lsp-value");

    assert_eq!(
        public_store.get(b"same-key"),
        Some(b"public-value".to_vec())
    );
    assert_eq!(lsp_store.get(b"same-key"), Some(b"lsp-value".to_vec()));
    assert_eq!(
        config.tenant_store_root(),
        PathBuf::from(root.path()).join("lsp/tenants")
    );
}

#[test]
fn lsp_service_rejects_public_store_reuse() {
    let public_store_path = PathBuf::from("shared/store");
    let config = LspConfig {
        base_dir: Some(PathBuf::from("shared")),
    };

    assert_eq!(
        config.validate_store_separation(&public_store_path),
        Err("LSP service store must be separate from the public Fiber store".to_string())
    );
}
