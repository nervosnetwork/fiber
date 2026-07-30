#![cfg(all(unix, not(feature = "sqlite")))]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use fiber_store::{StorageBackend, Store};
use nix::sys::stat::{umask, Mode};
use tempfile::tempdir;

const UMASK_TEST_CHILD: &str = "FNN_UMASK_TEST_CHILD";

#[test]
fn test_restrictive_umask_applies_to_new_rocksdb_files() {
    if std::env::var_os(UMASK_TEST_CHILD).is_none() {
        let status = Command::new(std::env::current_exe().expect("get current test executable"))
            .args([
                "--exact",
                "test_restrictive_umask_applies_to_new_rocksdb_files",
                "--nocapture",
            ])
            .env(UMASK_TEST_CHILD, "1")
            .status()
            .expect("run umask test child");
        assert!(status.success(), "umask test child failed");
        return;
    }

    // Apply the same process-wide policy as FNN before RocksDB creates any files.
    umask(Mode::from_bits_truncate(0o077));

    let dir = tempdir().expect("create temporary directory");
    let store_path = dir.path().join("store");
    let backup_path = dir.path().join("backup");
    let store = Store::open_db(&store_path).expect("open RocksDB");
    store.put(b"key", b"value");
    store.backup(&backup_path).expect("create checkpoint");

    let sst_files = fs::read_dir(&store_path)
        .expect("read store directory")
        .map(|entry| entry.expect("read store entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "sst"))
        .collect::<Vec<_>>();
    assert!(
        !sst_files.is_empty(),
        "checkpoint should flush at least one SST file"
    );

    assert_private_tree(&store_path);
    assert_private_tree(&backup_path);
}

fn assert_private_tree(path: &std::path::Path) {
    let metadata = fs::metadata(path).expect("read path metadata");
    let expected_mode = if metadata.is_dir() { 0o700 } else { 0o600 };
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        expected_mode,
        "unexpected permissions for {}",
        path.display()
    );

    if metadata.is_dir() {
        for entry in fs::read_dir(path).expect("read directory") {
            assert_private_tree(&entry.expect("read directory entry").path());
        }
    }
}
